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

So from Rust, RTEMS can do blocking sockets and `poll()`/`select()` readiness
today, but declares nothing mio's epoll/kqueue selectors could bind to. mio
also has no `poll`/`select` backend. That is the whole reason tokio-as-is does
not run on RTEMS.

**Correction (2026-07-22).** An earlier version of that sentence read "RTEMS
… **cannot** host mio's epoll/kqueue backends", which states a *platform*
limit from a *bindings* fact. The absence audited above is in rust-lang/libc's
newlib/RTEMS module, not in the OS: RTEMS 6 + libbsd has a working `kqueue`,
and §5.3 of `doc/rtems-scope-b-session-handoff.md` measures a libevent reactor
serving PVA end to end on this same BSP through it. "tokio-as-is does not build
for RTEMS" is true and is what this section supports. "A reactor cannot run on
RTEMS" is false, and nothing in this document should be read as asserting it.

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
  `event_config_avoid_method(conf, "kqueue")` under `#ifdef __rtems__`,
  `evhelper.cpp:183`).

→ PVA is reactor-based **even in C++**. "sync thread-per-client" does **not**
fit it, on any platform.

**Correction (2026-07-22).** An earlier version of that arrow continued: "Its
RTEMS path is a `select`-backed reactor (libevent itself has a select backend),
not a collapse to blocking threads." The second half stands; the first half is
wrong, and §5.3 of `doc/rtems-scope-b-session-handoff.md` is the measurement:

- The `#ifdef __rtems__` guard avoids `kqueue` only, and what this build
  actually lands on is **`poll`** — of the compiled-in backends
  (`EVENT__HAVE_KQUEUE`, `_POLL`, `_SELECT`; `EPOLL`/`DEVPOLL`/`EVENT_PORTS`
  all `#undef`). libevent's *select* backend was never tested here.
- That `poll` backend never blocks on this BSP: `poll()` returns `POLLERR`
  immediately on libevent's internal notify FIFO, so one 4.000 s loop issues
  148,081 `poll()` calls against 1 for a raw `poll()`, guest idle 33.6 %
  against 97.9 %. The comment on the guard dates it to "libbsd circa
  RTEMS 5.1"; it is what makes pvxs unusable on RTEMS 6 today.
- With that one line removed and nothing else changed, pvxs serves on RTEMS 6
  under `kqueue` — `pvxinfo`, `pvxget`, `pvxput`, `pvxmonitor` all end to end.

So the honest form of this section's consequence is: pvxs's RTEMS path is the
*same* reactor it runs everywhere, wedged into a broken backend by a stale
workaround. Our blocking driver avoids that class by not depending on a
reactor — not because a reactor cannot run there.

---

## 4. Decision (proposed)

Adopt a **thin runtime seam + per-platform drivers**, and reject a bespoke
runtime rewrite.

```
        ┌──────────────────────────────────────────────┐
        │      protocol cores (sans-io where it pays)   │   ← platform-agnostic
        │      CA state machine · PVA operations        │
        └───────────────┬──────────────────────────────┘
                        │  runtime seam (task/timer seam + net newtypes)
        ┌───────────────┴───────────────┬──────────────────────────┐
        │  desktop driver               │  RTEMS driver — ONE       │
        │  (tokio reactor)              │  blocking thread/conn     │
        │  Linux/macOS/Win/FreeBSD      │  + park_on + background   │
        │                               │  executor — CA *and* PVA  │
        └───────────────────────────────┴──────────────────────────┘
```

- **Desktop (Linux/macOS/Windows/FreeBSD):** keep tokio. It already works on
  all four, and it stays the default forever — see the dropped fallback below.
- **RTEMS — one backend for both protocols (decided 2026-07-21).** Blocking
  thread-per-client over RTEMS libc sockets (`std::net`), with `park_on`
  driving whatever async remains and the std-thread background executor behind
  the `runtime::task` seam. **CA and PVA share it.** There is no second RTEMS
  runtime design.
- **No `select()` reactor for PVA.** PVA needs *multiplexing*, not a reactor:
  many concurrent operations on one connection. The CA driver already supplies
  exactly that — `run_event_task` multiplexes N subscription futures plus a
  control channel through one `select_all` under a single `block_on_sync`, on
  one thread. PVA's per-operation tasks (monitor/get/put/RPC) mount on that same
  pattern, so the "new systems work" a select reactor implied is already built
  and exercised. This trades pvxs's one-reactor-for-N-connections for RSRV's
  threads-per-connection — an RTEMS task-count cost, deliberately accepted in
  exchange for a single backend.
- **Dropped: the `smol`/`async-io`/`polling` fallback** (was phase 6). Not
  merely unnecessary — *blocked*: the probe in §8.1 shows `polling` routes its
  syscalls through **rustix**, which hard-codes Linux signal constants absent
  from newlib (~181 errors). Adopting it would mean owning a rustix fork. The
  seam still permits a driver swap later if a *measured* desktop footprint
  target ever fails, but no such swap is planned and no work is reserved for it.

### 4.1 Rejected alternative: bespoke cross-platform runtime

Writing our own reactor+executor for all five platforms means, concretely,
**re-implementing mio** (epoll/kqueue/IOCP/poll-select) plus a scheduler,
timer wheel, async net types, and re-homing ~487 `tokio::sync::*` call sites —
then owning that runtime across five kernels forever. It would spend
person-years to replace a battle-tested runtime on the four platforms that
already work, in order to serve the one platform (RTEMS) — which, under the
single-backend decision above, needs **no reactor at all**: blocking sockets
plus `park_on` plus the background executor. Runtime bugs surface as rare
production hangs/races — the worst failure class for a controls system.
Cost/benefit is lopsided. With the `smol`/`polling` fallback dropped, this
alternative is rejected outright rather than held in reserve: nothing in the
plan now depends on replacing tokio on the desktop.

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
| RTEMS direction | collapse to **sync** thread-per-client | keep **async**, drive it on the CA backend |
| reactor needed? | **no** | **no** — see the revision below |
| db de-async needed? | **yes** (dominates cost) | **no** — keep async db under the executor |
| wire output split | must be built (~150 sites) | **already a channel** |
| monitor | redesign to sync callback | keep existing tasks |

**Revised 2026-07-21 — PVA reuses the CA backend; no select reactor.** The
earlier conclusion ("PVA's RTEMS bottleneck is a `select`-based reactor +
minimal executor") conflated two different needs. What PVA's spawned tasks
(**73 sites** — 67 `tokio::spawn` + 4 `JoinSet::spawn` + 2 already on the seam;
42 production / 28 test / 3 bins, scoped 2026-07-21) actually require is
**multiplexing many futures on few threads** — not
readiness notification over many sockets. The CA blocking driver already
delivers that: `run_event_task` drives N subscription futures plus a control
channel through one `select_all` under a single `block_on_sync` on one thread,
and `future_exec` + `park_on` behind the `runtime::task` seam run spawned
tails. PVA mounts on the same primitives:

- **reader thread** — blocking `read` → frame parse → hand the frame to the
  operation thread. **Correction (2026-07-21):** an earlier draft of this
  bullet said "→ `handle_message` (already a sync `fn`)". That was wrong.
  `handle_message` (`tcp.rs:6023`) is the leaf handler for PVA command 18
  (`MESSAGE`, a client log line), not the dispatcher. The real dispatcher is
  the inline `match Command::from_code(..)` at `tcp.rs:3790` and it is
  **async** (`tx.send(..).await`, `handle_create_channel(..).await`,
  `process_connection_validation(..).await`). So the reader thread does more
  work than that bullet implied — and one invariant must survive the split:
  `tcp.rs:3384-3395` makes the read loop the single owner of the inbound
  `TypeCache`, dispatching every frame synchronously in wire order. Frame
  *parsing* may move to the reader thread; **type-cache resolution must not**,
  or a `0xFD` define races a later `0xFE` reference;
- **writer thread** — blocking drain of that mpsc → `write_all` (the channel
  boundary PVA already has, `tcp.rs:3082/3215/3234`);
- **operation thread** — `block_on_sync` over the per-operation futures
  (monitor/get/put/RPC) multiplexed with `select_all` + a control channel,
  i.e. the `run_event_task` shape verbatim;
- **spawned tails** — the background executor via the task seam.

So PVA's RTEMS cost is now dominated by **wiring onto an existing backend**,
not by building a new one. PVA still does **not** need the db de-async (the
cross-crate blast radius that dominates CA's deep cut) — `block_on_sync`
awaits the existing db futures exactly as a select executor would have.
The accepted trade is task count: pvxs multiplexes N connections on one
libevent thread, whereas this runs threads per connection (the RSRV shape).
If a deployment's RTEMS task budget makes that bite, the fix is bounding
connection count or folding several connections onto one operation thread —
both local changes, not a return to the reactor design.

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
| **mio 1.2** (under tokio) | blocked (by design) | no epoll/kqueue; mio dropped its poll/select backend deliberately (thin epoll/kqueue/IOCP-only wrapper) | §4 — not needed: the single RTEMS backend is blocking thread-per-client for both CA and PVA, so no selector is ever constructed |
| **polling 3.11** (smol/async-io reactor; the mio alternative) | **FAILS — ~181 errors, but in `rustix`, not polling** | polling's OWN backend selection handles RTEMS (RTEMS is `unix` → its generic `poll()` backend, lib.rs:105-113). The blocker is transitive: polling routes syscalls through **rustix 1.1**, whose libc backend hard-codes Linux signal constants (`SIGSTKFLT`, `SIGPWR`, …) absent from newlib | Using the smol/polling stack on RTEMS requires porting **rustix** to newlib (upstream or fork). polling's design is RTEMS-ready; rustix is the wall. Verified 2026-07-20, isolated probe. |
| **socket2 0.5** (`features=["all"]`, **non-optional**; asyn/base/ca/pva) | **FAILS — 20 errors** | libc lacks `msghdr`/`recvmsg`/`sendmsg`/`IovLen` (scatter-gather), `ip_mreqn` (only `ip_mreq` exists), `ip_mreq_source`+`IP_{ADD,DROP}_SOURCE_MEMBERSHIP`, `SOCK_RAW`/`SOCK_RDM`/`SOCK_SEQPACKET`, `MSG_TRUNC`/`MSG_EOR`, `IPV6_RECVHOPLIMIT`/`IPV6_RECVTCLASS`/`IP_HDRINCL`/`IP_RECVTOS` | Most missing symbols are for APIs CA/PVA do **not** use (raw/seqpacket sockets, source-specific multicast, sendmsg/recvmsg). The subset they need — basic multicast join (`ip_mreq`, present), `setsockopt` for SO_REUSEADDR/buffer sizes — works over raw libc. Wrap that subset; do not port all of socket2. |
| **getrandom 0.2.17** (transitive: ed25519 / rustls / ahash …) | **FAILS — explicit `compile_error!("target is not supported")`** | RTEMS not in getrandom 0.2's supported-target list | RTEMS newlib *does* have `getentropy`/`arc4random_buf` → register a custom source (`register_custom_getrandom!`) or move to a getrandom version/backend that maps RTEMS onto them |
| **if-addrs 0.13.4** | **FAILS — 2 errors** | libc lacks `getifaddrs`/`freeifaddrs`/`ifaddrs` for rtems; the non-Apple path also needs Linux `sockaddr_nl`/`AF_NETLINK`/`NETLINK_ROUTE`/`SOCK_RAW` | Interface enumeration must use an RTEMS-specific route (libbsd `getifaddrs` via a `-sys` binding, or an RTEMS ioctl-based enumerator). Not a drop-in. |

Positive result from the same run: **`std` itself builds for
`armv7-rtems-eabihf`** via `-Z build-std` **without** an RTEMS gcc/BSP present —
so `cargo check` is a usable RTEMS gate on this machine today. The blockers are
all in these leaf crates, not in std.

**Ecosystem-wide finding (2026-07-20).** Every mainstream async-I/O dependency
probed — mio, socket2, getrandom, if-addrs, and polling→rustix — fails on
RTEMS. RTEMS async is therefore **not "pick the portable crate"; it is a
dependency-porting project under any strategy** (the reactor abstraction, mio
or polling, always bottoms out on a syscall-wrapper crate that hard-codes
Linux/BSD). This shifts the strategy trade-off decisively toward the **sync CA
path (§4 RTEMS/CA)**: a blocking thread-per-client CA over *raw RTEMS libc*
(blocking sockets + `poll()`) touches **none** of these crates, keeping the
RTEMS-specific cost internal and reviewable (our de-async) rather than external
and forever-maintained (a forked rustix/mio). **Extended 2026-07-21:** the same
reasoning is why PVA now shares that backend instead of getting its own
reactor (§4, §7) — a hand-rolled `poll()` reactor would have avoided rustix,
but reusing the blocking driver avoids writing a reactor at all. **RTEMS
confirmed as a committed target 2026-07-20 → the sync-CA deep cut (§6) is the
adopted path.**

### 8.1.0 The gate is `scripts/rtems-check.sh` (2026-07-22)

The invocation below is recorded here for its history; **the gate that runs is
`./scripts/rtems-check.sh`**, and that is the form to quote when reporting an
RTEMS check as green.

The prose form was `-p <crate> --lib`, and `--lib` never compiles
`src/bin/*.rs`. `realtime-ca-ioc` — the only binary anyone boots on the target —
was therefore outside every "RTEMS gate green" report on this branch, and a
build break introduced with `b594b18a` (E0433, a missing `StackSizeClass`
import) survived until the bring-up box tried to boot the image. The defect was
the gate's *scope*, not the code, which is the same shape as the
`--all-targets` blindness to `#![cfg(feature = …)]` test files.

Measured while fixing it, so the flag set is not a guess:

* `--bins` and `--all-targets` fail for this triple, and not in the linker:
  the host CLI tools (`caget-rs`, `caput-rs`, `camonitor-rs`, `softioc-rs`,
  `ca-admin-rs`, `ca-soak`) do not compile for RTEMS at all
  (E0432/E0433/E0308) and were never meant to.
* The narrowest set that covers the target is `--lib` per crate **plus**
  `--bin` per binary actually built for RTEMS.
* `cargo check` does not link, so no RTEMS toolchain or BSP is needed for any
  of it.
* The break also reproduces on the host with
  `cargo check -p epics-ca-rs --bin realtime-ca-ioc --features rtems-exec-model`.

The script fails if a `src/bin/*.rs` compiled for RTEMS is absent from its
list, so the next target binary cannot be added outside the gate silently.

### 8.1.1 Applied gating — `epics-base-rs` (2026-07-20)

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


### 8.1.2 Applied gating — `epics-pva-rs` (2026-07-21)

(The §8.1.1 counterpart — the same wall-walk for `epics-base-rs` — is above;
it landed with the CA branch.)

On branch `phase6/pva-rtems-dep-gate` (commits `bc7c8f53`, `8f12cf30`,
`73d3ec39`, `1906c7cb`, atop items 1/2), `cargo +nightly check -p epics-pva-rs
--lib --no-default-features -Zbuild-std=std,panic_abort --target
armv7-rtems-eabihf` reaches **exit 0 with zero warnings** — *when
epics-base-rs's §8.1.1 gating is present*. Walls hit and closed:

| Crate (ver) | Error class | Dependency path (from `epics-pva-rs`) | Gate applied |
|---|---|---|---|
| socket2 0.5 | 20 errs — no `msghdr`/`recvmsg`/`IovLen`, no `ip_mreqn`, no `SOCK_RAW` in newlib | direct **and** `socket2 ← tokio (net)` | direct dep → `[target.'cfg(not(target_os="rtems"))'.dependencies]`; users are `server_native::{udp,tcp}` only, gated with that layer |
| if-addrs 0.13.4 | 2 errs — no `getifaddrs`/`freeifaddrs`/`ifaddrs` | direct | dep → host-only; 3 of 4 call sites ride out with the I/O layer, the 4th does **not** (`Config::expand` NIC fan-out — configuration, not I/O; `Config` is *absent* on RTEMS rather than present-and-wrong, and the raw enumerator is owed with phase 6 item 7 since newlib bindings expose no `ifreq`/`ifconf`/`SIOCGIFCONF` to verify against) |
| mio 1.2.0 | 29 errs — no epoll/kqueue selector | `mio ← tokio (net)`, `tokio ← {epics-pva-rs, tokio-util}` | tokio declared per-target; RTEMS drops `net`/`signal`/`process` (per-target because Cargo *unions* features across matching tables) |
| signal-hook-registry 1.4.8 | 4 errs — no `sa_sigaction`/`SA_RESTART` | `signal-hook-registry ← tokio (signal)` | same split; `util::SigInt` keeps its API but traps nothing on RTEMS — the documented "platform without signal support" case |
| nix 0.31.3 | 21 errs | `nix ← rustyline ← epics-base-rs` | closed by base's §8.1.1 (CA branch), not here |
| *(in-crate)* libc `getifaddrs` | 3 errs — `cli::iface_name_to_ipv4` | newlib binding has no `ifaddrs` | guard `all(unix, not(target_os="rtems"))`; the name path returns an explicit "pass the interface's IPv4 address instead" — the answer non-Unix hosts already got |
| *(in-crate)* libc `getgrouplist` | 1 err — `auth::plain` | newlib lacks only this symbol of that path | RTEMS arm returns `vec![basegid]` (passwd primary group — the hosted loop's own fallback); `getuid`/`getpwuid`/`getgrgid` all exist, so the group still comes back named |

Every gate is `target_os = "rtems"`, never a feature: it is not a choice a
hosted build can make, and cannot be flipped on by accident.

**Compile surface (measured):** of 96,875 non-bin src lines, **44,858 (46%)
compile for RTEMS**; 28,164 (29%) are target-gated (`tcp` 21,415 + `udp` 3,486
+ `runtime` 2,181 + `peers` 485 + `server::pva_server` 452 + `server::iocsh`
145); 23,853 (25%) are the client feature. The RTEMS surface is codec + config
+ the source/`SharedPV`/DB-bridge layer — the sans-io half that the phase-6
item-7 blocking driver plugs into. De-asyncing the 21k-line `tcp.rs` protocol
engine is separate work (item 5).

**Integration note — the CA branch is now the critical path.** On this branch
*as committed* the RTEMS check exits 101 with exactly the five base-side walls,
because `epics-pva-rs`'s own edges to socket2/if-addrs are gone (verified:
`cargo tree -p epics-pva-rs --target armv7-rtems-eabihf -i` shows one inbound
edge each, through `epics-base-rs`) but base's gating lives unmerged on the CA
branch. Phase-6 items 5/8/9 likewise depend on that branch's primitives
(`run_event_task`, the `runtime::task` seam, `future_exec`). Merging
`caucus/WG0SFREHPX/ca-sans-io-1962c8be-1` into main is the user's decision, but
until it lands, no PVA RTEMS milestone can be *committed* green.

### 8.2 Gateable — excluded from the RTEMS build

- **ring / rustls / tokio-rustls (TLS)** — `optional = true` in **epics-ca-rs
  only**, behind `experimental-rust-tls` (`epics-ca-rs/Cargo.toml:28,79`).
  **Correction (2026-07-21):** the claim that "the base CA/PVA build does not
  pull ring" was wrong for PVA. In `epics-pva-rs/Cargo.toml:36-38`
  `rustls`/`rustls-pemfile`/`tokio-rustls` are **non-optional**, so
  `cargo tree -i ring` resolves through them and drags **getrandom 0.2.17** —
  the crate that `compile_error!`s on RTEMS. Same for `socket2 0.5`
  (`:20`) and `if-addrs 0.13` (`:29`), also non-optional there, and
  `config/env.rs:320/1326/1368` calls `get_if_addrs` on the **server** path so
  it cannot be gated away with the client. A TLS/feature gate on epics-pva-rs
  is therefore a **phase-6 prerequisite, not later cleanup**. For CA the
  original statement stands. TLS on RTEMS proper is still a separate, later
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
5. **RTEMS entry point.** ✅ **DONE 2026-07-21** (`15f1fc6c`,
   `crates/epics-ca-rs/src/bin/realtime-ca-ioc.rs`). Starts `background_init()`,
   a database via `IocBuilder` under `block_on_sync`, then `BlockingCaServer`
   on `CAS-TCP`/`CAS-UDP` threads — no tokio runtime. **The feared regression
   did not happen:** `cargo +nightly check --bin realtime-ca-ioc
   --target armv7-rtems-eabihf` exits 0 and the bin's `--extern` set contains
   **no socket2, no if-addrs, no getrandom, no mio** — the §8.1.1 `--lib`
   gating holds unchanged at binary scope. (The `[[bin]]` deliberately carries
   no `required-features`: that would make cargo *skip* the target and turn the
   RTEMS gate into a vacuous pass.) Host proof: real `caget`/`camonitor` over
   127.0.0.1, and `ps -T` shows zero tokio worker threads.
6. **PVA on the CA backend** (revised 2026-07-21; was "`select` reactor +
   minimal executor"). Mount PVA's reader/writer/operation threads on the
   existing blocking driver primitives per §7. No new runtime is designed or
   built; this phase is wiring plus the RTEMS dependency work PVA adds on top
   of CA's.

   **Scope: PVA SERVER only (decided 2026-07-21).** A PVA *gateway* on RTEMS
   is explicitly out of scope — it would need a reader/writer/heartbeat trio
   per upstream connection plus the UDP search engine, flipping the 12
   `client_native` production spawn sites from "gate out" to "restructure" and
   roughly doubling the phase. Estimate at this scope: **~7–9 engineer-weeks**,
   ordered so the desktop-neutral items land first and shrink the big one:

   | # | item | size | status |
   |---|---|---|---|
   | 1 | TLS/dep feature gate on `epics-pva-rs` (§8.2 correction) | 3–5 d | ✅ `1d5476df` |
   | 2 | Split `client_native::decode`; gate client modules + `client_config()` | 2–4 d | ✅ `24d514e8` |
   | 3 | *(renumbered into §8.1.2)* dep walls: socket2/if-addrs/tokio split/getgrouplist | — | ✅ `bc7c8f53`..`1906c7cb` |
   | 4 | `MonitorInbox::try_recv` + widen `ChannelSource`; delete 6 bridge tasks | ~~4–6 d~~ 1–2 wk | ✅ `aeb41927`, `bbd74e6f`, `72239333` |
   | 5 | **reader/operation/writer split** — design settled 2026-07-21, see `doc/pva-rtems-item5-design.md`; **stages 1–2 LANDED 2026-07-21** on `phase6/pva-channelsource-ring` (`ad268398` `runtime::task::timeout` seam + `93590517` 11 spawns + 3 timers onto the seam, production `tokio::spawn` in `tcp.rs` = 0 pinned by a fail-closed source-inspection test; workspace 9757 green; hosted-neutral — the seam delegates to tokio off-RTEMS). ~~One owed rename post-merge: `AbortOnDrop`/`abort` field (`tcp.rs:783`/`:819`) still name `tokio::task::AbortHandle` because the `TaskAbortHandle` alias exists only cfg-gated on the CA branch — an ungated alias here would collide at merge~~ rename **landed `04cdf6fa`** after the merge | ~~2–3 wk~~ **3–4.5 wk** total, **DONE** | ✅ **ALL 5 STAGES LANDED** (closed 2026-07-23: stage 3 `44681c76` blocking three-thread driver, stage 4 `ab97461f` ConnRegistry + `shutdown(Both)` teardown — §4.2b's `oneshot` arm dissolved by socket-wake, hosted timing untouched; stage 5 closed by the executable gate `scripts/rtems-check.sh`, both configurations, `--extern` set recorded; byte source promoted to `runtime::blocking_io` by `8024b175`; as built in `doc/pva-rtems-item5-design.md` §§10–12). Design-time shape notes, kept: **shape revised**: NO op-future boxing (the boxing answered the worker-per-future defect that `d704087d` deleted — ops become 11 `runtime::task::spawn` swaps); reader hands **bytes not frames** via a new `SrvRead` implementor (TypeCache invariant holds in place, now at `tcp.rs:3418-3428`); writer = thread with a deadline loop over `write()` (SO_SNDTIMEO alone is per-syscall, strictly weaker than the per-frame bound `tcp.rs:3262-3273` documents); shutdown owner = operation thread, needs an explicit socket registry + `shutdown(Both)` because PVA's ~64,000 s `op_timeout` (`:3488`) cannot double as the wake mechanism the way CA's 45 s one does. 5 stages, each workspace-green; 3 decisions flagged before stage 3, one needing user sign-off (a `oneshot` writer-death arm that also speeds *hosted* teardown) |
   | 6 | Fold heartbeat + monitor-gate driver into the operation loop | 2–3 d | ✅ `f0ca0909`, `e278088c` — both per-connection helper tasks folded into the read loop's select; two structural collapses came free (`last_rx` Arc→local, watch receiver clone gone). Same shape remains in the PVA *client* (`client_native/server_conn.rs:634-679`) — out of scope by the server-only decision, named not forgotten |
   | 7 | Blocking accept/UDP/beacon threads + raw-libc subset — **scoped 2026-07-21, see `doc/pva-rtems-item7-design.md`.** Reshaped: not "swap a backend under a compiled module" — the dep gate (`8f12cf30`) deliberately left the whole PVA server absent on RTEMS and item 7 supplies it. `tcp.rs` is 21,656 lines with an **8-line socket surface**, all inside `run_tcp_server*` (`:2432-2727`) → **stage A (split accept loop into `server_native/accept.rs`) is a shared prerequisite of items 5 AND 7** and must be pulled ahead of item 5's stage 3. 7 stages: A+B (accept split + socket-free UDP search core, the PVA analogue of CA `ad477153`) hosted-neutral ≈ 1 wk; C+D+E+G (blocking accept/UDP/beacon drivers + `realtime-pva-ioc`) ≈ 2.5–3 wk blocked on the CA merge + item 5; F (NIC enumerator) BSP-blocked, unsized. FIONREAD coalescing is CA-specific — pvxs sends one reply per datagram (`udp.rs:1048-1053`), porting it would be a wire deviation. NIC enumeration (`SIOCGIFCONF`/`ifconf`/`ifreq`/`getifaddrs`) is the one real libc blocker; recommendation = explicit address list required on RTEMS with a loud startup refusal (option b), BSP `ifconf` walk as a later increment. **4 decisions need sign-off** (§7 of the doc): per-NIC vs single-socket UDP replies; auto-beacon strategy; TCP keepalive via raw `setsockopt` (recommended, constants present) vs omit; merge ordering — **three unmerged branches are on item 7's critical path** (CA branch, `pva-channelsource-ring`, `pva-rtems-dep-gate`; item 7 cannot build on cs-ring because TLS deps are non-optional there → mio → RTEMS wall) | ~~1 wk~~ **3.5–4 wk** schedulable + BSP-blocked F | **stages A+B LANDED 2026-07-21** on `phase6/pva-channelsource-ring` (`4c75e766` accept loop → `server_native/accept.rs`, moved block diff-verified byte-identical, `tcp.rs` socket surface now **0** pinned by a mutation-proved guard, gate re-point 4 cfg lines owed at merge and named in the commit message; `aa1af842` `process_search_datagram` socket-free → `Vec<SearchOutput>` {Reply, OriginTagForward}, send tail lifted unchanged to `dispatch_search_outputs`, 5 wire-golden UDP e2e byte-unchanged; workspace 9760 green). Stages C–G remain blocked on the merge order |
   | 8 | Re-home 9 timer sites; 2 socket timeouts → `SO_{SND,RCV}TIMEO` | 2–3 d | blocked on CA-branch merge |
   | 9 | 87 `abort()` sites → `TaskHandle` (abort maps — see §11) | 1 wk | blocked on CA-branch merge |
   | 10 | RTEMS `--lib` green + QEMU monitor-to-completion | 1 wk | `--lib` proven with base overlay (§8.1.2); committed-green blocked on CA-branch merge |

   Items 1–4 landed 2026-07-21 on `phase6/pva-rtems-dep-gate` (items 1–3) and
   `phase6/pva-channelsource-ring` (item 4), both off main, both
   workspace-green (9743 and 9751 tests respectively), neither merged. Item 4's
   outcome: **all six bridge tasks deleted** — `tokio::spawn` count in
   `shared_pv.rs`/`server/native_source.rs`/`server_native/source.rs` is zero;
   one MONITOR on a db-backed PVA IOC now costs 1 task, not 2–3. The three
   transform bridges were *replaced* by `UpstreamMonitor` (owns the Db/Pv
   subscription, applies the transform on pull; empty-mask filter and
   connect-time seed preserved), and mapped-of-mapped is unrepresentable by
   type, which is what keeps the default monitor path allocation-free.

   Items 1/2/4 are **desktop-neutral** — they improve the hosted build too and
   can land before any RTEMS wiring. Item 4 alone deletes 6 tasks: today a
   single MONITOR on a db-backed PVA IOC costs 2–3 tasks because
   `ChannelSource` pins the return type to `mpsc::Receiver` and a bridge task
   copies the ring into it (`shared_pv.rs:1428-1445`). Its trait change ripples
   into `epics-bridge-rs`, so item 4 requires `nextest --workspace`, not
   per-crate.

   **Item 4 re-measured 2026-07-21 — the 4–6 d figure was wrong by roughly an
   order of magnitude.** Two facts found while starting it:

   - **3 copies + 3 transforms, not 6 pure copies.** `shared_pv.rs:1434`,
     `:1477` and `source.rs:1687` are pure copies. But
     `native_source.rs:878/927/957` pull from `DbSubscription`/`PvSubscription`
     and *transform* — `snapshot_to_pv_field`, `nt::event_leaves` marking with
     an empty-mask `continue` filter, and an initial seed send at `:927`.
     Deleting those three needs a lazy adapter owning the upstream
     subscription and applying the transform inside its own `recv`/`try_recv`,
     not a signature change. Enabler: both subscriptions are backed by
     `EventReader`, which already has `try_recv` (`event_queue.rs:570`), so the
     non-blocking path exists all the way down and only needs exposing.
   - **112-site sweep across 2 crates**, not the 5 methods named in
     `source.rs`: 81 sites in epics-pva-rs + 39 in epics-bridge-rs matching
     `Option<mpsc::Receiver<(PvField|RawMonitorEvent|MonitorUpdate)>>`, plus the
     public `SubscriptionSeed::updates` field; `ChannelSource` has 77 impls
     workspace-wide. Mitigating: consumers need only `recv().await` and
     `try_recv()`; the epics-bridge-rs middleware only *forwards* streams and
     constructs no monitor channels, so those 39 are type-name swaps; and
     `MonitorInbox`/`MonitorOutbox` are confined to `shared_pv.rs`.

   Design to settle **before** the sweep, so it lands in one piece rather than
   leaving the tree red: generic `MonitorRing<T>`; `MonitorStream<T>` as an
   allocation-free enum `{Channel, Ring}`; `try_recv` exposed on
   `Db`/`PvSubscription`; concrete adapter variants for the three
   `native_source` transforms.

   `MonitorInbox::try_recv` itself is **done** (`aeb41927` on
   `phase6/pva-channelsource-ring`), with per-boundary tests: empty ring, one
   item, squashed tail, `producer_done` with items still queued,
   `producer_done` with an empty ring, and pending-credit decrement.

The old phase 6 (`smol`/`polling` desktop-driver fallback) is **dropped
outright** — see §4. The number is reused above by the PVA phase; there is no
fallback phase in this plan any more.

Phases 1–5 deliver a working RTEMS **CA** IOC — the cheaper, higher-value
half — before PVA is wired onto the same backend in phase 6.

---

## 10. Success criteria (verifiable)

- Desktop: `cargo nextest run --workspace` stays green throughout; no behaviour
  change on Linux/macOS/Windows/FreeBSD (the tokio driver is the default).
- CA sans-io: the CA protocol core has tests that assert wire bytes from
  `(hdr, payload, state)` **without** any socket/`DuplexStream`.
- RTEMS CA: `cargo check --target armv7-rtems-eabihf` on the CA server crate +
  its deps builds; a blocking thread-per-client IOC accepts a CA connection and
  serves a `caget`/monitor under QEMU.
- PVA RTEMS: an existing PVA monitor operation runs to completion under QEMU on
  the **same** blocking backend as CA (no separate reactor in the build).

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
- ~~Whether the `polling`/`async-io` stack can host an RTEMS `poll`/`select`
  backend today.~~ **MOOT 2026-07-21** — phase 6 dropped (§4). The probe answer
  is recorded in §8.1 anyway: `polling`'s own backend selection is RTEMS-ready,
  but it routes syscalls through **rustix**, which fails on newlib (~181
  errors, hard-coded Linux signal constants). Adopting the stack would mean
  owning a rustix fork.
- ~~Exact `smol` migration cost if phase 6 fires: `broadcast` (41 sites) /
  `watch` (14) / `Notify` (64) have no drop-in in the `async-*` crates.~~
  **MOOT 2026-07-21** — phase 6 dropped; tokio stays the desktop driver.
- ~~Whether the background executor can **abort a parked future** — the single
  largest cost swing in the phase-6 estimate (87 `abort()` sites in
  epics-pva-rs rely on tokio cancel-at-await-point).~~ **CLOSED 2026-07-21: it
  can.** `future_exec` exposes `JoinFuture::abort` / `AbortHandle` /
  `JoinError::is_cancelled`; `request_abort` latches the flag then unparks the
  driving worker, and the driver runs
  `park_on_interruptible(fut, || control.abort.load(Acquire))`
  (`future_exec.rs:321`, `:352`), so a *parked* future does observe the abort
  on wake. Semantics match tokio's: cancellation is observed at the next await
  point, not mid-stretch. So `AbortOnDrop` guards port as a rename rather than
  a redesign.
- **New (open):** the RTEMS **task-count budget** under the single-backend
  decision. CA and PVA both run threads-per-connection, so peak RTEMS task
  count is a function of concurrent client connections. Not yet measured
  against a BSP's configured task limit — this is the one cost the
  single-backend choice adds relative to the dropped reactor design.
  **Materially improved 2026-07-21** by two changes: `future_exec` is now a
  cooperative executor (`d704087d` on the CA branch — a spawned future no
  longer parks a pool worker for its life; N sleeping tails share one band
  worker, proven by `three_sleeping_tails_share_one_worker`), and PVA's
  per-connection helper tasks are folded away (items 4+6: one MONITOR = 1
  task, heartbeat and gate driver are select arms, not tasks). The budget
  still needs measuring, but the per-connection multiplier is now bounded by
  design rather than by workload.
  **Narrowed 2026-07-22 by measurement on the box:** the *connection* ceiling
  is now known. Two walls bind — the descriptor cap (142 CA connections
  served, #143 refused with `ENFILE`) and heap (151 served on an image whose
  cap was raised to 400, refused with `EAGAIN` on thread creation) — so the
  effective ceiling is 142 and raising the cap alone buys nine. Full record,
  including how our cap deviates (we run base's *score*-arm 150 on a target
  where base compiles the *POSIX* arm and runs 64), how to override it without
  a source edit, and what an operator can watch:
  **`doc/rtems-fd-ceiling-deviation.md`**. *Inferred, not
  separately measured:* neither observed refusal was an RTEMS object-table
  exhaustion, and `CONFIGURE_UNLIMITED_OBJECTS` is set
  (`csrc/rtems_config.c`), so the configured task limit was not the binding
  constraint in these runs — but the task count itself was still never
  counted, and that is what stays open here.
- ~~The `rtems-exec-model` feature-ON full `epics-ca-rs` suite is red.~~
  **CLOSED 2026-07-21 (`aea5d73d`): the suite is now a usable gate — 569
  tests, exit 0 on four consecutive runs** (three by the implementer, one by
  the verifier). Classification, not blanket gating: 25 modules/files gated
  with why-comments (the CLI subprocess suites, the in-process async stack,
  and per-test gates preserving the 13 pure wire-format tests in mixed files);
  the red population was dominated by the async **server**
  (`server/tcp.rs:1336`, `server/udp.rs:771`, `server/ca_server.rs:1099`),
  not the client as first assumed — same cause, same rule. One category-(b)
  genuine bug found and fixed at source rather than gated:
  `event_watcher_tests` raced a `yield_now` spin against band workers with no
  happens-before edge (13/20 failures on the OLD executor — the race predated
  the rewrite). Remaining gaps: (1) ~~`blocking_rtems_e2e` had to be gated
  because its *client* half is async — `async_write_notify_rtems_exec` is the
  feature's only e2e until a blocking-client e2e exists~~ **CLOSED 2026-07-21
  (`6f5f492f`)**: `blocking_raw_client_e2e.rs` — 4 raw-socket tests (no async
  `CaClient`, zero `.await`), feature-neutral so it guards BOTH backends
  (feature-ON 573, feature-OFF 695): full command round-trip incl. a delivered
  monitor update, EVENT_CANCEL zero-payload ack (C `camessage.c:2002-2014`
  shape), UDP search advertising the real TCP port + a circuit dialed from the
  advert, accept-loop survival across client teardown. Still open within it:
  the raw-socket e2e seeds a `SimplePv`, not `IocBuilder` records — a
  real-record blocking e2e under the feature does not exist yet
  (`async_write_notify_rtems_exec` covers the async-record path) —
  **CLOSED 2026-07-21 (`bfaae8f3`)**: `blocking_real_record_e2e.rs`, 5
  feature-ON tests (calcout ODLY runs on the executor; new discriminating
  property: the blocking driver's message thread keeps serving the circuit
  while a put-callback is pending — C `camsgtask` parity, fails with ODLY 0);
  (2) ~~the `interop` profile under the feature is unmeasured~~ **CLOSED
  2026-07-21 (`051b7851`)**: measured with the C tools on PATH +
  `--run-ignored all` — feature-OFF 20/20; feature-ON 10 reds, **all
  reactor-dependent** (`there is no reactor running` ×18 on `cbMedium`
  workers), **zero wire-compatibility differences between the two backends**;
  the three async-driving files gated with why-banners, `wire_golden_ext`'s 9
  pure wire tests left in so the profile is green (9/9), not empty. Open
  residue: the census guard cannot see *child-process* reactor dependency
  (`interop_c_client_rust_ioc.rs` spawns `softioc-rs` from plain `#[test]`s;
  a `CARGO_BIN_EXE_` anchor was evaluated and rejected — it false-positives
  on CLI arg-parsing tests); (3) ~~the gate is convention, not mechanically
  enforced~~ **CLOSED 2026-07-21 (`d9807d75`)**: `rtems_exec_model_gate.rs` —
  a census (not allowlist: declared count must equal found count, so the
  second async test in a waived file still fires), two anchors
  (`#[tokio::test]` + hand-built `runtime::Builder`), mutation-proved, 23
  files declared. Test-client hardening (`055d7d11`): the raw CA test client
  is now a `Circuit` with a pending-frame queue — the discarding read
  primitives are deleted so the lucky-order defect class cannot return one
  call site at a time; measured order split (real-record path replies
  WRITE_NOTIFY-first only 10/25) and a deterministic fake-peer race test pin
  it.
- The CA deep-cut estimate depends on how many of the ~226 `snapshot()`/`set()`
  call-sites are already outside an async context (auto-reconciled by de-async)
  vs. need rewriting — not yet classified per-file.

---

## 12. One-line summary

Do **not** rewrite the runtime. Put a thin seam under the protocol cores; keep
tokio on the four platforms that already work; give RTEMS **one** driver —
blocking thread-per-client sockets plus `park_on` and a std-thread background
executor — and run **both CA and PVA** on it. CA mirrors RSRV exactly; PVA
gives up pvxs's single-reactor shape in exchange for reusing a backend that is
already built and exercised, paying in RTEMS task count rather than in a second
runtime design.
