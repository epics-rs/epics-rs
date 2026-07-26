# RTEMS scope-B — session handoff

Written 2026-07-22 to carry a multi-panel caucus session across machines. The
session is moving **to `192.168.2.128`**, the RTEMS/QEMU box, which until now
was a remote target driven over ssh and becomes the local machine.

Read this top to bottom before resuming. It is written so a fresh session does
not have to re-derive anything — several facts below cost hours of qemu time,
and at least three of them **overturned a conclusion reached from source
reading**. Where a belief was wrong, the wrong version is recorded too, because
the wrong version is what a fresh reader will otherwise re-derive.

**Naming note (2026-07-25).** The target IOC binaries were later renamed —
`rtems-ca-ioc` → `realtime-ca-ioc`, `rtems-pva-ioc` → `realtime-pva-ioc`.
Every old name below is left exactly as captured, because this file is a
record of the tree as it stood, not a description of it as it stands.

---

## 1. What the work is

Make the CA and PVA servers of `epics-rs` run on RTEMS 6, on a **single
blocking thread-per-client backend** — `std::net` plus `park_on` plus a
std-thread background executor. No async reactor, no tokio runtime on the
target. Hosted (Linux) behaviour unchanged throughout.

Upstream splits its I/O model **by protocol**: EPICS base's RSRV is
thread-per-client, pvxs is a libevent reactor. We split **by platform** — one
sans-io core, a blocking driver on RTEMS and the async driver on hosts.

### There are two axes here, and they are easy to collapse into one

| | CA — EPICS base | PVA — pvxs |
|---|---|---|
| **Linux** | thread-per-client, blocking `recv()` | libevent reactor → **epoll** |
| **RTEMS 6** | thread-per-client, blocking `recv()` | libevent reactor → **kqueue** (once §5.3's one line is removed) |
| **`epics-rs`, RTEMS 6** | blocking thread-per-client | **blocking thread-per-client** |

- **The protocol axis (columns) is a real design difference.** CA blocks a
  thread per client; PVA multiplexes in a reactor. Upstream chose that, and it
  holds on every platform.
- **The platform axis (rows) is not a design difference at all.** `epoll` and
  `kqueue` are the *same* libevent reactor with a different backend compiled in.
  pvxs's source does not name either; libevent picks from what the platform
  offers. On this RTEMS build the compiled-in candidates are exactly kqueue,
  poll, select — `EVENT__HAVE_EPOLL` is `#undef`.

Two corrections worth stating explicitly, because both were believed here first:

- **"CA uses kqueue" is false on every platform.** RSRV has *no* multiplexing
  call at all: `rg '\b(select|poll|kevent|kqueue|epoll_wait)\s*\('` over
  `modules/database/src/ioc/rsrv/*.c` returns **zero hits**. It spawns a
  `CAS-client` thread per connection (`caservertask.c:109`) which blocks in
  `recv()` (`camsgtask.c:71`). That thread pair is what §5.2 measures. libca is
  the same shape — the single `select()` that greps in `tcpiiu.cpp:2062` is
  inside `#if 0`, with `osiSockIoctl_t bytesPending` live instead.
- **"pvxs switched models on RTEMS" is also false.** It is the same reactor; only
  the backend differs, and the RTEMS wedge of §5.3 happens entirely inside the
  bottom-right cell.

Our row puts **both** protocols in the left-hand model, which is why the RTEMS
target never has to choose a reactor backend. §5.3 is what that is worth, stated
at its measured strength and no higher: a libevent reactor **does** work on
RTEMS 6 once steered away from the `poll` backend, so "a reactor cannot run
there" is false. What is true is that the reference implementation ships an
RTEMS-5-era workaround that steers itself into a broken backend, and finding
that took an interposer and CPU-idle attribution. Our backend does not depend on
a reactor, so it never meets that class of defect. That is the claim to make.

### What the blocking-thread model is, and what it does not give you for free

The one-line difference is **who does the waiting**.

- **Blocking thread** — one thread per connection, parked in `recv()`. The
  *kernel scheduler* is the multiplexer. Application code runs top to bottom;
  connection state lives on the thread's stack.
- **`select`/`poll`** — one thread hands the kernel the whole fd list, wakes,
  and **rescans all N** to find who is ready. O(N), list re-copied per call.
- **`epoll`/`kqueue`** — the interest set is *registered* once; the kernel
  returns only what is ready. O(ready). Both are the same idea, Linux's and
  BSD's; kqueue additionally carries timers, signals and process events on the
  same queue. Choosing between them is not a design decision — it is whichever
  one the platform has.

The real dividing line is blocking versus the other three, not select versus
epoll. With a reactor the connection state cannot live on a stack, because
there is no per-connection stack — hence state machines, callbacks, async.

**What the blocking model wins, in this project's terms:**

| | blocking thread | reactor |
|---|---|---|
| per-connection memory | **1,589,000 B** (§5.6; 97.4 % stack) | a small state object |
| dependency on OS readiness plumbing | none | total — §5.3 is what that costs |
| one connection stalls the others | no | yes, for the duration of a callback |
| priority granularity | per connection | per loop (see §5.9) |
| priority inheritance possible | yes | **structurally not** (§5.9) |

Memory is the only column where blocking loses, and it loses badly: that
1.5 MiB is what makes the ceiling of §5.1. Everything else favours it *for an
IOC*, whose connection count is tens, not tens of thousands. C has run CA this
way for thirty years — `rg '\b(select|poll|kevent|kqueue|epoll_wait)\s*\('`
over `modules/database/src/ioc/rsrv/*.c` returns **zero hits**.

**Grep trap, both directions.** The same `rg` over our two blocking drivers
also returns zero *syscalls* — but it does hit `select!`, which is the tokio
futures macro, not `select(2)`. And async code is still present: `park_on`
drives futures inline on the connection thread (13 sites in the CA driver
alone). "Blocking driver" means no reactor, not no async.

**And the model does not hand you priority for free.** It only makes
per-connection priority *possible*. Whether it materialises depends on two
further things — that priorities are actually applied, and that a blocked
thread is blocked on something the kernel can attribute an owner to. On this
target neither currently holds. §5.9.

---

## 2. Repo state

**Branch `integration/rtems-scope-b`, 149 commits ahead of `main`, tip
`56c58661`. Never pushed to any remote.** It lives in a git worktree at
`.caucus/worktrees/integration` on the dev machine, off `main@5241145f`.

Gates green at that tip: `cargo nextest run --workspace`,
`cargo clippy --workspace --all-targets -- -D warnings`,
`./scripts/rtems-check.sh`, `cargo doc` clean for `epics-base-rs`,
`epics-ca-rs`, `epics-pva-rs`.

### Getting commits to the box — use a bundle, not a push

This is the established route and it needs no shared remote and no public push:

```sh
git bundle create scopeb.bundle <box-tip>..HEAD
scp scopeb.bundle coding-agent@192.168.2.128:~/
ssh coding-agent@192.168.2.128 \
    'cd ~/epics-rs && git fetch ~/scopeb.bundle "+HEAD:refs/heads/scope-b-incoming"'
```

`git fetch` from a bundle does **not** touch the working tree — the box's
`~/epics-rs` carries deliberate local modifications (instrumented sources, a
`Cargo.lock` libc pin) that must survive. Once the session runs *on* the box
this inverts: the dev machine becomes the remote.

### The RTEMS gate is `./scripts/rtems-check.sh` — run it, do not hand-type it

Two crates' worth of history says why. The gate used to live in prose as
`cargo check --target armv7-rtems-eabihf -p <crate> --lib`, and `--lib` never
compiles `src/bin/*.rs`. `rtems-ca-ioc` — the only binary anyone boots on the
target — was outside the gate for the whole branch, and an `E0433` survived
every "RTEMS green" report until the box tried to boot it.

The script carries a **census**: every `src/bin/*.rs` built for RTEMS must
appear in its `BINS` list or the gate fails. Measured facts behind its shape:
`--bins` fails on the host CLI tools, which legitimately do not compile for
RTEMS; `--all-targets` dies in `ring`'s build script under `arm-rtems6-gcc`
via a dev-dependency; `cargo check` never links on any selector
(`--emit=dep-info,metadata`, no executable emitted). `required-features` was
**rejected** as an alternative because `epics-ca-rs/Cargo.toml:239-244` warns
it makes cargo silently *skip* the target — a worse failure than the one being
closed.

**Both known defects are closed, and the second one's diagnosis above was
wrong.** Recorded because the wrong cause is the more plausible-looking one:

* The guard was never excluded by `--no-default-features`. `epics-rtems-boot`
  declares no `[features]` at all, so that flag cannot reach it — and the flag
  is load-bearing for an unrelated reason that must not be undone
  (`epics-pva-rs`'s default `tls` drags `ring` → `getrandom 0.2`, which
  `compile_error!`s on the target). What excluded the guard is `cfg(all(
  target_os = "rtems", rtems_boot_linked))`, and `rtems_boot_linked` is emitted
  by `epics-rtems-boot/build.rs` **only when `RTEMS_BSP_PREFIX` is set** — i.e.
  never on a machine without a BSP, which is every machine this gate was
  written for. The axis missed was not *what* is compiled but *which
  configuration*. The gate now compiles both (`CONFIGS=(portability image)`,
  the image one selected with `RUSTFLAGS=--cfg rtems_boot_linked`, no source
  cfg touched), and the image pass is fatal.
* `M Cargo.lock` was not the script's doing. Measured: a full run at
  `7a88c6a8` leaves the tree clean. The rewrite comes from the
  `[patch.crates-io] libc` a linking build needs, and the old gate exited **0**
  while accepting it — reporting on a resolution nobody committed. `--locked`
  makes that a named error. Pinning `libc` was measured and rejected: under
  `--precise 0.2.188` **both** layout refusals still fire, identical to
  0.2.186. No published `libc` satisfies the predicates.

---

## 3. The box — `ssh coding-agent@192.168.2.128`

The user's own desktop. Passwordless sudo, with an explicit instruction not to
abuse it.

**Security contract, restated to every agent that touches it:** sudo ONLY for
`apt-get install`; run `-s` first and abort if the dry run shows `Upgrade:` or
`Remv:`; then `--no-install-recommends`. Never remove/purge/upgrade/
dist-upgrade/autoremove, no PPAs, no apt source edits, nothing under `/etc`, no
`systemctl`, no reboots, never kill a process you did not start, never touch
`/home/stevek` or another user's files, write nothing outside `$HOME` except
what apt does.

**Directory contract.** Two agents already collided over one checkout, so
ownership is explicit:

| path | owner | contents |
|---|---|---|
| `~/epics-rs` | bring-up | the Rust tree; carries deliberate local mods |
| `~/rtems-bringup/` | bring-up | images, boot scripts, backups, `libc-bringup` clone |
| `~/pvaprobe/` | bring-up | box-local PVA link probe |
| `~/rtems-cside/` | C-side | upstream base + pvxs + libevent, `DEVIATIONS.md` |
| `~/rtems-bringup/libc` | **the user** | see §7 |

Cross-panel rule: clone or copy into your own directory, never work in
another's. Guests use disjoint ports — bring-up 5064/5075/15076, C-side
5164/5174/5175/5176.

Environment: RTEMS 6.0.0 `2faafecb`, gcc 13.3.0, newlib `1b3dcfd`, BSP
`xilinx_zynq_a9_qemu` under `qemu-system-arm`, 256 MB guest. QEMU invocation
`-serial null -serial mon:stdio`, `-nic user,` (onboard GEM). SLIRP hostfwd is
sufficient for CA and PVA — no tap, no sudo. Two forwarding traps, both
measured, both in §5.4.

---

## 4. Panels

Four, all `claude`/`opus`. Roles are stable; recreate with these charters.

| role | job |
|---|---|
| `w1-desync` | the only panel that writes to the repo. Every commit on the branch is its work (plus two of mine). |
| `rtems-qemu-bringup` | builds images, boots guests, measures on target. Owns `~/epics-rs`, `~/rtems-bringup`. |
| `async-tail-research` | read-only source analysis and design docs. Writes to `scratchpad/`. Never edits, never builds. |
| `c-side-rtems` | cross-builds **upstream C/C++** for RTEMS and runs it, so comparisons are measurements rather than readings of C source. Owns `~/rtems-cside`. |

The `c-side-rtems` charter is worth preserving verbatim in spirit: it is a
*measurement* role whose output is numbers and console transcripts plus honest
statements of what could not be measured; every deviation it applies to stock
upstream source is declared, because an undeclared patch contaminates the
comparison.

Every panel's end-of-turn report uses **Tested / Failed / UNFIXED / Fixed**,
each case on its own line, no aggregation.

---

## 5. Measured facts

Everything here was measured on the target unless marked otherwise. These are
the expensive results.

### 5.1 The connection ceiling is BSP configuration, not our design

Identical driver (raw CA TCP, version handshake, one `CA_PROTO_CREATE_CHAN`,
"served" only on reply `18`) against both stacks:

| build | `CONFIGURE_MAXIMUM_FILE_DESCRIPTORS` | last served | first refused |
|---|---|---|---|
| stock EPICS base | **64** (base ships this) | 53 | #54 |
| base, matched to our guest | 150 | 139 | #140 |
| `epics-rs` | 150 | **142** | #143 |

Same console line on both, same errno:

```
[zone: socket] kern.ipc.maxsockets limit reached
CAS: Client accept ERROR: Too many open files in system      # ENFILE
```

C is **3 lower** than us. Stock base ships fd=64; our guest's 150 is **our**
deviation and should be documented as one. Note base's own comment at
`modules/libcom/RTEMS/posix/rtems_config.c:83`: raising the cap past
`FD_SETSIZE` faults any code calling `select()`, and libca/RSRV do not.

**There are two separate walls and 142 is not the memory one.** This is worth
stating flatly because the two are easy to conflate:

| | set by | our guest | if RAM doubles |
|---|---|---|---|
| **fd wall** | `MAXIMUM_FILE_DESCRIPTORS − 8` | **142** | unchanged |
| **memory wall** | free heap ÷ 1,589,000 B | **151** | roughly doubles |

142 is arithmetic, not a memory result: the cap is 150 and the IOC itself holds
8 descriptors at idle, which §5.7's status PVs confirm directly — `FD_CNT +
FD_FREE = FD_MAX = 150` on every row, and `FD_FREE` reads exactly **142** at
zero connections. The errno says the same thing: `ENFILE`, not `ENOMEM`. C's
numbers are the same arithmetic — stock base stops at 53 from a cap of 64, and
is 3 behind us at the same cap only because it holds 3 more descriptors itself.

The memory wall was measured separately, on an fd=400 image where the fd wall
no longer binds: **151 served**, refusals `EAGAIN` (thread creation) rather than
`ENFILE`, matching `(259,803,736 − 19,880,696)/1,589,000 = 150.99`.

**The effective ceiling is the lower of the two, so raising either alone buys
almost nothing** — the cap buys 142→151, and more RAM buys nothing at all while
the cap is 150. The lever that would actually move both is per-connection
memory, 97.4 % of which is the two thread stacks (§5.2) — and §5.2 says C
over-provisions those exactly as we do.

### 5.2 Stack classes — settled, do not reopen

C rsrv on the identical target, `rt stackuse` at 139 held connections:

| thread | requested | high-water |
|---|---|---|
| `CAS-client` (per connection) | 1,048,560 B | **2,024 B** |
| `CAS-event` (per connection) | 524,272 B | **380 B** |

**C requests exactly what we request — Big + Medium — and over-provisions by
~650×.** Base's `stackSizeTable` is not sized for the connection body on this
target either. Cutting our classes would be a deviation from what C measurably
does, not a correction of one.

Our own peaks (RTEMS stack checker, pattern-filled, reported **on the
connection thread at its exit** — a 1 Hz timer sampler was tried first and
caught *zero* live connection threads in 54 reports, because connections die
inside one interval):

| role | peak | workload |
|---|---|---|
| CA receiver | **22,084 B** | held monitors on C1..C8 + depth-8 FLNK chain |
| CA sender | 2,928 B | held monitors pushing 800 KB updates |
| PVA conn | **21,320 B** | all ops concurrent, 60 s |
| PVA reader | 5,568 B | |
| PVA writer | 2,552 B | |

**The CA receiver is linear in database link depth: ≈1,584 B per FLNK level,
base ≈7,792 B** (depth 1/3/5/7/8 → 9,376 / 12,608 / 15,712 / 18,880 / 20,464).
The dominant term is *user database data*, not a protocol quantity. `Big`
exhausts at ~656 links, `Small` at ~160.

Headroom against `Small` is 11.9× (CA receiver) and 12.3× (PVA conn). **RTEMS
POSIX threads have no guard page**, so being short is silent corruption of a
neighbour's stack, not a fault.

> **The belief that was wrong:** "under 7 KB used, so cut the classes, 8.1×
> more connections." The 7 KB came from an idle probe and was wrong at depth 1;
> the 8.1× came from a `RUST_MIN_STACK` simulation that modelled Small/Small
> when the real classes are Big/Medium. `StackSizeClass::bytes()` is
> **unchanged** and its documented parity with the POSIX `stackSizeTable`
> stays true.

Also settled: `task.rs:452-458` does **not** forbid measured deviation. It
forbids one specific substitution — swapping the POSIX table for the RTEMS-
*score* table (5000/8000/11000) — on a parity argument about which C file an
RTEMS 6 build compiles (`configure/toolchain.c:31-36`: `__RTEMS_MAJOR__>=5` ⟹
`OS_API = posix`).

### 5.3 pvxs on RTEMS 6 — root-caused, and it works after one line

This section was rewritten twice. **Both earlier versions were wrong, and the
panel that wrote them retracted them under measurement.** The wrong versions
are kept because they are what a fresh reader will otherwise re-derive.

> **Wrong v1:** "pvxs does not run on RTEMS 6." → It does.
> **Wrong v2:** "`event_base_loop` on a secondary pthread busy-spins." → On a
> secondary pthread the loop is fine. The thread was never the variable.

**The actual defect: libevent's `poll` backend never blocks on this BSP, on
any thread**, because `poll()` returns `POLLERR` immediately on libevent's
internal notify descriptor. Measured with `-Wl,--wrap=poll` around one 4.000 s
loop (probe binary only; libevent and pvxs untouched):

| backend | `poll()` calls in 4 s | guest IDLE |
|---|---:|---:|
| raw `poll()` | 1 | 97.9 % |
| libevent kqueue | 0 | 97.7 % |
| libevent **poll** | **148,081** | **33.6 %** |

The caller sees a correct 4 s block; the loop thread burns the core for all of
it. The descriptor, isolated with no libevent involved:

```
pipe() -> fd 56   [IMFS FIFO]   poll(POLLIN,1000ms) rv=1 revents=0x8 in 0.0002s  <- POLLERR
socketpair()-> 58 [unix socket] poll(POLLIN,1000ms) rv=0 revents=0x0 in 1.0058s  <- blocks
udp socket -> 55                poll(POLLIN,1000ms) rv=0 revents=0x0 in 1.0106s  <- blocks
```

`evutil_make_internal_pipe_()` prefers `pipe()` over `socketpair()`, and
`pipe()` here is an RTEMS IMFS FIFO, which libbsd's `poll()` flags as an error
instead of waiting on.

**The priority hypothesis was tested and rejected as the cause.** With the loop
thread explicitly below the other (`SCHED_FIFO` 3 vs 1) the starvation vanishes
— but guest IDLE is 0.000074 s of 4.017 s. The core is still fully consumed.
Priority separation hides the symptom. Equal-priority single-core `SCHED_FIFO`,
which `pthread_create` produces by inheritance, is only why it *presents* as a
hang.

**What else libevent could have used.** From the actual build's
`bundle/O.RTEMS-xilinx_zynq_a9_qemu/include/event2/event-config.h`, three
backends are compiled in and no more — `EVENT__HAVE_KQUEUE`,
`EVENT__HAVE_POLL`, `EVENT__HAVE_SELECT`; `EPOLL`, `DEVPOLL` and `EVENT_PORTS`
are all `#undef`. kqueue is measured working (table above, plus peer-close
detection). **The select backend was never tested** — the raw `select(1 udp fd)`
in the table is a syscall probe, not libevent's select backend, and the fd that
breaks is the internal notify FIFO, not a socket. If libbsd's `select()` folds
`POLLERR` into "readable" the way FreeBSD's does, it would spin identically;
that is a hypothesis, not a result. One `avoid_method("poll")` added to the
existing probe would settle it.

**Owner: rtems-libbsd.** `poll()` on a valid open non-socket fd must report
real readiness, not `POLLERR`. libevent deserves a defensive change (prefer
`socketpair()` on RTEMS — measured to work). pvxs is the victim but holds the
one-line workaround: `src/evhelper.cpp:183`
`#ifdef __rtems__ event_config_avoid_method(conf,"kqueue")`, written for
"libbsd circa RTEMS 5.1", is what steers it into the broken backend.

**With that one line commented out and nothing else changed, pvxs serves on
RTEMS 6:** `iocRun: All initialization complete`; `pvxinfo` returns full
NTScalar introspection; `pvxget PIOC:AI1` → `1.25`; `pvxput` then `pvxget` →
`7.5`; `pvxmonitor` streams from a 0.1 s scan record. And the RTEMS-5.1 reason
for the workaround does not reproduce — `SIGKILL`ing a monitor client (no FIN)
is detected under kqueue: `connection to Client … closed by peer`.

**What this means for our design rationale — state it accurately, not
conveniently.** Our blocking thread-per-connection PVA backend is *not* "the
only thing that runs on RTEMS", which is what the previous version of this
document claimed and what a brief already told a panel to write into the code.
A libevent reactor does work on RTEMS 6 once steered to `kqueue`. The honest
claim is narrower and still real: **the reference implementation ships an
RTEMS-5-era workaround that makes it unusable on RTEMS 6 today**, and it took a
`--wrap=poll` interposer and CPU-idle attribution to find that. Our backend
avoids the whole class by not depending on a reactor. Write that, not the
stronger version.

Full write-up on the box: `~/rtems-cside/FINDING-1-libevent-poll-spin.md`.

### 5.4 The `libc` time defect is ours alone — clean control

C IOC, same BSP, 60.056 s host wall clock:

```
HB100  601 ticks -> 10.0073 Hz     (SCAN=".1 second")
HB1000  60 ticks ->  0.9991 Hz
```

No quantization, no missed timeouts. On-target: `sizeof(time_t)=8`,
`sizeof(struct timespec)=16`, with a negative control that fails to compile
when asserting `==4`. C uses the toolchain's own newlib headers and is
unaffected.

The Rust `libc` crate declares `time_t = i32` for `armv7-rtems-eabihf`, so
`libc::timespec` is 8 bytes where RTEMS's is 16. Consequences, all measured:
every `clock_gettime` writes 12 bytes into an 8-byte stack slot; `tv_nsec`
lands where the 8-byte view never reads it, so **every `Instant` reads 0 ns**
and the clock is 1-second-quantized, not frozen; and `pthread_cond_timedwait`
panics std's `assert!`, so **every sub-second `Condvar::wait_timeout` never
wakes** while whole seconds work.

Related target facts: `try_clone`/`F_DUPFD` fails `ENXIO` on any libbsd socket
(share `Arc<TcpStream>`; and dup never gave fds separate socket timeouts
anyway); `setsockopt` with `optlen=8` is cleanly rejected `EINVAL` by libbsd;
every `std::thread` leaks 128 B on RTEMS (TLS key freed before its destructor
runs — raw C pthreads leak 0); `getentropy` works, so PVA GUIDs are fresh per
boot.

### 5.5 qemu forwarding traps

1. **The TCP host port must equal the guest port.** `hostfwd=tcp::15075-:5075`
   lets search succeed and then `Connection refused` — the client follows the
   port the server advertises. `5075-:5075` works. Same for CA 5064.
2. **The UDP host port must not be 5076** — that is the port a PVA *client*
   needs for itself; they collide silently under `SO_REUSEPORT`. Use
   `hostfwd=udp::15076-:5076` with `EPICS_PVA_ADDR_LIST=127.0.0.1:15076`.

Both TCP forwards live simultaneously. Bind forwards to `127.0.0.1` — qemu's
default exposed 5064/5075 on the LAN.

PVA is reachable over **TCP alone** via `EPICS_PVA_NAME_SERVERS`, no UDP or
broadcast — which is what makes it testable under SLIRP.

Verification tooling: use **pvxs**, not our own client, against our own server.
`pvxinfo` for identity, `pvxget -F tree` for a value, `pvxlist -w 5 -v` for the
GUID. Default Delta output omits the top-level struct id, so it cannot
demonstrate the served type.

### 5.6 Memory — flat on both stacks, and raising the fd cap is not worth it

> **Wrong version, recorded:** "C's non-stack heap grows non-linearly, ~8 KB at
> 53 connections and ~42 KB at 139." That was arithmetic across two different
> builds and two different boots. The panel retracted it. There is no growth.

C, one boot, four marks, reproduced across two boots to 0.1 MiB:

| connections | heap free | incremental per connection |
|---:|---:|---:|
| 0 | 219.7 MiB | – |
| 53 | 138.0 MiB | 1,616,383 B |
| 100 | 65.6 MiB | 1,615,251 B |
| 139 | 5.6 MiB | 1,613,183 B |

Ours, fd cap raised to 400: 1,588,393 / 1,589,254 / 1,588,876 / 1,589,431 B at
25 / 50 / 100 / 140 connections — **spread 1,038 B, 0.065 %.** An independent
derivation from the ramp gives 1,589,013.

**Both stacks are flat and they agree.** `netstat -m` is byte-identical at 100
and 139 connections; diffing every UMA zone over 86 connections accounts for
1,654 B — 3.9 % of the residual. C's residual above the stacks is ~42.4 KB at
*every* count, and **77 % of it is rsrv's pair of 16 KB buffers**
(`caservertask.c:1284,:1287`, `MAX_TCP=16384` measured on target).

**97.4 % of per-connection cost is the two thread stacks** — the ones measured
at 2,024 B and 380 B used (§5.2).

**Raising the descriptor cap is not worth proposing on this guest.** At fd=400
the fd ceiling stops binding and memory binds at **151 served**, by two
independent derivations (300 attempted with 149 refused; and
(259,803,736 − 19,880,696)/1,589,000 = 150.99). Refusals are `EAGAIN`, announced
on powers of two. So the cap buys **9 connections, 142 → 151**, and then the
256 MB guest is out of heap.

Repeat fill-and-drain is safe: residual ~300 B per connection across two
cycles, consistent with the known 128 B-per-`std::thread` RTEMS leak across two
threads. C reuses rather than leaks too — `casr 4` shows free-lists holding
4,773,576 B after 139 connections close; the 227 MiB is parked, not returned.

Scope limit, stated: these are VERSION-handshake holds with no channels or
subscriptions, so this measures the connection object, not per-channel state.

### 5.7 The IOC's own status PVs predict the ceiling exactly

`rtems-pva-ioc` publishes devIocStats-named PVs through a one-second pusher
thread (a `ReadHook` would not work — it is GET-only, so `camonitor` on a
hook-backed PV never updates). Verified with `caget` **and** `camonitor`: 23
updates each in 22 s, values actually moving.

| held | FD_CNT | FD_FREE | CA_CONN_CNT | MEM_FREE |
|---:|---:|---:|---:|---:|
| 0 | 8 | **142** | 0 | 241,199,000 |
| 100 | 108 | 42 | 100 | 82,313,800 |
| 141 | 149 | 1 | 141 | 17,148,200 |
| 142 | *unreadable* | — | — | — |

`FD_CNT + FD_FREE = FD_MAX = 150` at every row, one descriptor per connection,
and **`FD_FREE` at idle is numerically the ceiling.** A console-less operator
can watch it count down.

Two caveats that operator must be told, both measured:

- **The instrument dies at the wall.** At 142 held, `caget` returns nothing — a
  CA client needs a descriptor and there are none. You can see the wall coming;
  you cannot read anything once you hit it.
- **`CA_REFUSED_CNT` is the wrong alarm for this wall.** It stayed **0** through
  the entire ramp. The fd wall is an `accept` failure (ENFILE) that happens
  before a client object exists, so it never reaches the refusal counter.
  `FD_FREE` is the only published number that sees it.

Timestamps read `2014-04-14` — no RTC on target, so they are the RTEMS epoch
base, not wall clock.

### 5.8 `child_thread_lost`'s spawn arm is unreachable by construction

Not "undriven" — unreachable. The PVA connection thread is `Big` (1 MiB) and is
spawned **first, at the accept site**; `spawn_child` (reader/writer, 256 KiB
each) runs from inside `serve_connection_blocking`, which only executes if that
1 MiB allocation succeeded. Reaching `blocking.rs:483` therefore needs a heap
where 1 MiB allocates but 256 KiB does not — impossible in a single-heap
allocator. Verified at fd=400 with 320 held connections and
`free.largest=505,144`: the parent spawn failed cleanly, the guest stayed alive
and kept accepting. Its only reachable trigger today is the panic arm, which
*was* driven on a real console.

### 5.9 Priority — the axis where blocking should win

> **Superseded, 2026-07-22.** This section used to say priority was "dead three
> ways" on RTEMS. Two of those three are closed in the tree and the third was
> never what it was described as. What each reason said, and what is true now,
> is under *Status of the three reasons* below — **read that before acting on
> anything else in this section**, because the paragraphs after it were written
> against the old state. What remains open is the *lock* half (the wait
> discipline and PI), not the *thread* half.

This is the one property the blocking model buys that a reactor cannot.

**Why a reactor structurally cannot do priority inheritance.** In a reactor the
entity waiting on a contended resource is a *callback in a queue*. The kernel
cannot see it and there is no priority to inherit. pvxs runs every server
connection on one thread (`PVXTCP`, `server.cpp:388`, `CAServerLow-2` = 18), so
all connections share one priority and none can preempt another;
`event_priority_set()` orders the loop's own queue but never preempts a running
callback. A blocking thread parked in `recv()` has none of these limits — which
is the whole argument, and it is why the rest of this section matters.

#### Status of the three reasons

**Reason 1 — "RT priority cannot be switched on, on this target." CLOSED by
`d3dbb785`.** `DEFAULT_POLICY` is now a function of the target
(`task.rs:584-596`): `AllowRealtime` on RTEMS, `Disabled` on hosted.
`EPICS_RS_ALLOW_RT_PRIORITY` still overrides in *both* directions, so
`=NO` turns it off on target. The old reasoning — that `POSIX_Init` calls
`setenv` zero times and there is no shell, so an env-var opt-in is unreachable
— was correct; the fix was to stop making the target's default depend on an
env var it cannot set. Note the consequence for diagnosis:
`sched_calls_made() == 0` is no longer the correct reading on RTEMS, it is now
a symptom.

**Reason 2 — "even switched on, it would do nothing." CLOSED by `52784cb4`.**
`apply_priority_impl` has a live `#[cfg(target_os = "rtems")]` arm
(`task.rs:1113-1147`) that maps through `map_epics_priority_rtems` and calls
`pthread_setschedparam` via its own `rtems_sched` extern block — declared
locally because `libc`'s `newlib/rtems` module declares neither the function
nor `sched_param`. `Cargo.toml:79-80` links `libc` under `cfg(unix)`, not
linux-only. There is deliberately no range probe on this arm: RTEMS performs no
privilege check on `pthread_setschedparam`, and the map's image is a fixed
`[56, 155]` inside the measured settable `[1, 254]`. The map is the RTEMS-score
one (`posix = 56 + epics`), which is a *deliberate deviation* from
base-on-RTEMS-6's linear posix map — see `doc/pi-lock-evaluation.md` and the
comment on `map_epics_priority_rtems`.

**Reason 3 — "the baseline is at the bottom." TRUE, and it is not a defect to
fix at the shim.** `POSIX_Init` lowers itself to `RTEMS_MAXIMUM_PRIORITY - 1`
(`epics-rtems-boot/csrc/rtems_init.c:236`) and RTEMS pthreads inherit their
creator's parameters (`cpukit/posix/src/pthreadattrdefault.c:49-58`; `std`
never calls `pthread_attr_setinheritsched`). Base is identical here — it lowers
to the same value at `libcom/RTEMS/posix/rtems_init.c:1038`, never raises it,
and calls `main()` from it — and escapes the consequence not by raising the
baseline but by setting `PTHREAD_EXPLICIT_SCHED` at every thread creation
(`libcom/src/osi/os/posix/osdThread.c:158-166`). `std::thread::Builder` cannot
do that, so **the equivalent is that every thread takes its own band as its
first statement**, which is what `runtime::task::enter_ioc_thread` is.

So Reason 3 is not "priority is dead"; it is "a thread that skips the prologue
does not run at a default, it runs one level above idle." The question it turns
into is a census: which threads skip it.

#### The census — every thread that exists on the target

From the two binaries the gate builds (`rtems-ca-ioc`, `rtems-pva-ioc`), every
spawn reachable from them.

Rows marked **(pooled)** are no longer spawned per connection: both servers
borrow a connection's threads as one set from a `WorkerPool`, created once per
worker from the roster cited and reused
(`doc/rtems-connection-worker-pool-design.md`). Three consequences for reading
this census: the name carries the **worker index**, not the peer (`CAS-client 3`,
not `CAS-client-blocking 10.0.0.1:44312` — which is how target measurements taken
before the conversion spell the same thread); the band is stated in the roster
and applied by the pool at worker creation rather than by the thread body, so
the prologue cannot be skipped for these; and the row count is a bound on
concurrent connections, not on connections ever served.

| thread | site | band |
|---|---|---|
| `main` (`POSIX_Init` → `main`) | — | inherits `MAXIMUM_PRIORITY-1` — **correct**, see below |
| `cbLow` / `cbMedium` / `cbHigh` | `background/callback_executor.rs:293`→`:301` | `enter_ioc_thread(prio.os_priority())` |
| `cbTimer` | `background/delayed_timer.rs:232`→`:249` | `ScanHigh` |
| `scanOnce` | `background/scan_once.rs:176`→`:185` | `ScanLow` |
| `status-pv` | `server/status_pv.rs:291` | `Low`, via `spawn_dedicated_thread` |
| `CAS-TCP` | `ca .../blocking.rs`, `serve` | `CAS_TCP_PRIORITY` = 18 |
| `CAS-UDP` | `ca .../blocking.rs`, `serve_udp_search` | `CAS_UDP_PRIORITY` = 16 |
| `CAS-client <n>` | `ca .../blocking.rs:249` `client_roster` (pooled) | `CaServerLow` = 20 |
| `CAS-event <n>` | `ca .../blocking.rs:249` `client_roster` (pooled) | 19 |
| `PVAS-TCP` | `pva .../blocking.rs`, `serve` | `PVA_SERVER_PRIORITY` = 18 |
| `PVAS-UDP` | `pva .../blocking.rs`, `serve_udp_search` | `PVA_UDP_PRIORITY` = 16 |
| `PVAS-conn <n>` | `pva .../blocking.rs:645` `connection_roster` (pooled) | 18 |
| `PVAS-reader` / `PVAS-writer <n>` | `pva .../blocking.rs:645` `connection_roster` (pooled) | 18 |

Not in the closure, and why: `iocsh-startup` / `iocsh-after-ioc-running`
(`server/ioc_app.rs:696`,`:1040`) — neither RTEMS binary constructs an
`IocApp`, they build through `IocBuilder`; and they are one-shot script
runners, which in C run on the init task at exactly this baseline anyway.
`cbRtProbe` (`task.rs:818`) — `#[cfg(target_os = "linux")]`.

**`CAS-TCP` and `CAS-UDP` were the whole of the inherits-near-idle set**, and
they were closed on this branch. They called `name_current_thread()` alone,
documented as "deliberately unbanded". C bands both: `rsrv` builds one
descending ladder from `epicsThreadPriorityCAServerLow`
(`caservertask.c:562-575`), which on RTEMS is exactly `p-1`
(`RTEMS-score/osdThread.c:120-131`) giving `20, 19, 18, 17, 16`, and takes
`threadPrios[2]` for `CAS-TCP` (`:716`) and `threadPrios[4]` for `CAS-UDP`
(`:722`) — the same 18 and 16 the PVA side already used for the same roles. The
rule is now uniform with no unbanded exception, and the source guard in
`ca .../blocking.rs` pins that (four `enter_ioc_thread`, zero
`name_current_thread`, zero `apply_to_current_thread` in production scope).

**The `main` thread stays at `MAXIMUM_PRIORITY-1` on purpose.** It is the one
thread nobody creates, so no spawn-site owner can cover it — and it does not
need one: after it has started the IOC it only `join`s, and C's init task,
which goes on to run iocsh, sits at the same level for the same reason. Do not
"fix" it by raising the shim's lowering: that lowering is base parity and it is
what lets libbsd's background work outrank the init task.

**Invariant, and its single owner.** MUST: every OS thread that runs IOC work
on the RTEMS target takes its scheduling band *and* its OS name through
`runtime::task::enter_ioc_thread`, on the thread itself, before it runs any
work. MUST NOT: any such thread run at the priority inherited from
`POSIX_Init`. Owner: `enter_ioc_thread`. It is reached two ways, and both make
the band unforgettable by signature — `spawn_dedicated_thread`, which takes the
`ThreadPriority` as a parameter, and a hand-rolled `thread::Builder` whose body
calls it first. The hand-rolled sites exist for reasons that are load-bearing
(the `CAS-client` socket-handover channel needs to know spawn failed before the
socket moves), so they are classified *through owner*, not eliminated.

#### Still open: the locks

**Half the locks escape priority regardless.** Full table:
`doc/pi-lock-evaluation.md` (main `3406721d`). `runtime/sync.rs:3` re-exports
tokio's `Mutex`/`RwLock`, so **14 of 25 shared locks are async locks**. A
blocking thread reaching one via `block_on_sync` → `park_on` sits in
`std::thread::park()` (`task.rs:80`): no kernel-visible owner pointer, so no PI
chain — and tokio's FIFO fair wake replaces C's `RTEMS_PRIORITY` priority-
ordered queue, so even the *wait order* ignores priority. The worst case is L1,
the `dbScanLock` analogue (`database/record_lock.rs:70`), which C protects with
`epicsMutexMustCreate` and PI on (`dbLock.c:86`). It returns an
`OwnedMutexGuard` **by design** so callers can hold it across awaits
(`record_lock.rs:110`,`:121`) — which is exactly what blocks converting it.

**The mechanism itself is alive — priorities work when actually set.** §5.3's
control: giving the libevent loop thread `SCHED_FIFO` 3 against the other's 1
made the starvation disappear. That experiment is now the *supporting* evidence
rather than a caveat: it shows the kernel honours an explicitly set band, which
is exactly the path `enter_ioc_thread` takes.

**It takes effect on target. MEASURED, 2026-07-22.** A boot of `rtems-ca-ioc`
from `scope-b-priority` (`e89599431`) with two CA clients held, reading every
thread's band back from the kernel with `pthread_getschedparam` rather than
trusting what `enter_ioc_thread` returned, gives `SCHED_FIFO` and exactly
`posix = 56 + epics` on every banded thread: `CAS-client` 76, `CAS-event` 75,
`CAS-TCP` 74, `CAS-UDP` 72, `scanOnce` 116, `cbTimer` 126, `status-pv` 66,
`cbLow`/`cbMedium`/`cbHigh` 115/120/127, against `main` at posix 1 (core 254).
Neither failure mode §8.0 named applies — the threads are not all equal at the
baseline, and the absolute values are the intended ones, not merely the right
spread. Full listing, the libbsd band, and the raw console output:
`doc/rtems-priority-on-target-measurement.md`.

**One caution from that same experiment.** With priorities separated, guest idle
was **0.000074 s of 4.017 s**. Priority divides CPU; it does not create any.
It hid the symptom and left the defect. Do not reach for priority as a fix for
something that is burning the core.

---

## 6. Four upstream bugs found, none filed

All four are characterised well enough to file. Filing them is the highest-
value unstarted work in this session — three of the four are other people's
bugs that block anyone doing EPICS on RTEMS 6, not just us.

1. **rtems-libbsd — `poll()` returns `POLLERR` on a valid IMFS FIFO.** §5.3.
   The root cause of the pvxs wedge. Evidence: `-Wl,--wrap=poll` counts,
   CPU-idle attribution, and a three-descriptor discrimination test showing
   `pipe()` fails where `socketpair()` and a UDP socket both block correctly.

2. **pvxs — the RTEMS kqueue avoidance is an RTEMS-5.1 leftover that makes it
   unusable on RTEMS 6.** `src/evhelper.cpp:183`. Removing it yields a fully
   working PVA IOC, and the peer-close behaviour it was written to work around
   does not reproduce. This is a one-line fix with an end-to-end demonstration
   behind it.

3. **pvxs — RTEMS support targets RTEMS 5 in the build system too.**
   `bundle/cmake/Platform/RTEMS.cmake:28` passes `-specs bsp_specs`; RTEMS 6
   does not install that file (`-qrtems` alone is what base uses). Without
   removing it, nothing compiles.

4. **EPICS base — boot crash on its own documented configuration.** Reproduced
   on **unpatched** base with a 12-line application. Fault: `R0 = 0x00000000`,
   `R1 = 0x2f` (`'/'`), `PC` → `strchr`, `newlib/libc/string/strchr.c:100`,
   reached from `strrchr`; `main()` never entered. Chain, all in
   `modules/libcom/RTEMS/posix/rtems_init.c`:

   | line | what |
   |---|---|
   | 948 | `char *argv[3] = { NULL, NULL, NULL };` |
   | 216-217 | the `epicsRtemsFSImage==NULL` branch returns `0` **without assigning `argv[1]`** — every other success path assigns it (224, 256, 315, 339, 366, 411) |
   | 238 | `initialize_local_filesystem` treats hook-returns-0 as success |
   | 1164 | `set_directory(argv[1])` with `argv[1] == NULL` |
   | 471 | `cp = strrchr(commandline, '/');` → fault. The guard at 472 handles "no slash", not "no string". |

   Fix: assign `argv[1] = "/"` in the NULL branch at 216-217 (keeps the
   invariant where it is established) **and** make the consumer total —
   `cp = commandline ? strrchr(commandline,'/') : NULL;`. Both are cheap; doing
   both leaves no faulting configuration. Write-up and reproducer on the box at
   `~/rtems-cside/FINDING-2-base-rtems-fsimage-null.md` and `~/rtems-cside/fsbug/`.

Plus the Rust `libc` defects of §5.4, which are already filed as #5307/#5308 —
see §7 for why those are blocked on something other than their technical
content.

---

## 7. Open decisions — the user's, not an agent's

**libc PRs #5307 and #5308 (rust-lang/libc, fork `physwkim/libc`).** CI is
57/57 SUCCESS on both; the code is correct and §5.4 is the proof. But the
maintainer `tgross35` objected on all three touchpoints:

- #5307 — "This is not in a reviewable state. **Handwrite all comments, PR
  descriptions, commit messages** …" → `@rustbot author`
- #5308 — "#5132 is going to merge first … **No AI for PR descriptions, commit
  messages, or comments please.**" → `@rustbot blocked`
- the comment on #5132 — "**Do not post AI-generated blobs to communicate to
  humans.**"

Also: that comment restated a defect `tgross35` had **already** raised in review
comment `r3622267705` ("when is the `rtems` arm ever being hit? … our only
rtems platform is arm?"). The technical finding was right and not new; existing
review comments were not checked before posting.

The user is reworking these by hand — branch `rework/5307` in
`~/rtems-bringup/libc` on the box is **theirs**, and a cherry-pick there caused
a 20-minute window where every RTEMS image build hard-failed on the
`_RTEMS_LIBC_TIME_LAYOUT` guard because that branch lacks the `time_t`
widening. **Do not touch that checkout. Do not write PR prose. Do not push any
libc branch.**

Unpushed on the box, and a real fix that belongs in whatever the user files: an
`unused import: crate::prelude::*` warning introduced by gating `clock_t`,
fixed on all three branches (`bringup 6f64e70d6`, `rtems-type-widths
9499a21cd`, `rtems-type-widths-0.2 45c684a68`). libc CI does not build
`armv7-rtems-eabihf`, so it never caught it.

**Merge of `integration/rtems-scope-b`.** 149 commits, never pushed. The user
decides.

---

## 8. Next — what to do when the session resumes

### 8.0 The goal: reach C's blocking-thread level

Stated as a checklist, not as prose, so it can be closed rather than discussed.
**The blocking driver on RTEMS should be indistinguishable from RSRV's thread
model in every property that matters, and different only where the difference
is declared and better.** Current state, with every row labelled by how it is
known — measured on a boot, or read out of the source:

| property | C — RSRV | us | |
|---|---|---|---|
| threads per connection | 2 — `CAS-client` Big, `CAS-event` Medium | CA 2 — Big + Medium | **parity** |
| stack classes requested | Big + Medium | Big + Medium (§5.2) | **parity** |
| descriptors per connection | 1 | 1 (§5.7) | **parity** |
| multiplexing syscalls | 0 | 0 | **parity** |
| per-thread leak | 0 (raw pthreads) | **128 B** | **gap 1** |
| thread priority requested | `PTHREAD_EXPLICIT_SCHED` at creation, every thread | every thread, via `enter_ioc_thread` (§5.9 census) | **parity in source** |
| thread priority *observed on target* | **measured 2026-07-22** — live, 18 distinct levels over 26 threads; `CAS-client` posix 51 / core 204, `CAS-event` 49/206, `CAS-TCP` 46/209, `CAS-UDP` 41/214 | **measured 2026-07-22** — `SCHED_FIFO`, `posix = 56 + epics` on every thread; `CAS-client` 76/179, `CAS-event` 75/180, `CAS-TCP` 74/181, `CAS-UDP` 72/183 | **gaps 1 and 2 CLOSED** — same EPICS numbers on both sides, the whole divergence is the map |
| EPICS → POSIX map | `(int)(epics × 2.53 + 1)`, range min 1 / max 254 — reproduces all 20 measured rows with no residual; the score arm's `199 − epics` matches none | `posix = 56 + epics` — confirmed on all 8 threads | **deliberate deviation**, now measured on both sides rather than assumed |
| scan/callback band vs libbsd | **7 threads above `_BSD` (100), `scan-1` (EPICS 63 → core 95) above `IRQS` (96)** — measured | cannot happen: map image is core `100..=199`, and the most urgent live thread (`cbHigh`) is core 128 — measured | **deliberate, better** |
| lock wait discipline | `RTEMS_PRIORITY` ordered | tokio FIFO fair | **gap 3** |
| PI on the scan lock | on — `epicsMutexMustCreate`, `dbLock.c:86` | none — L1 is `park_on`-invisible | **gap 4** |
| refusal at the ceiling | accept, then **zero bytes**, then FIN | `CA_PROTO_ERROR`/`ECA_ALLOCMEM`, announced on powers of two | **deliberate, better** |

Four gaps, in dependency order — each is only worth doing if the one above it
is done, because otherwise it is unobservable:

1. ~~**Measure whether C's own thread priorities take effect on RTEMS 6.**~~
   **DONE 2026-07-22 — write-up and full console transcript at
   [`doc/upstream-rtems-bugs/measurement-c-thread-priority-on-rtems-6.md`](upstream-rtems-bugs/measurement-c-thread-priority-on-rtems-6.md).**
   **C is not inert**, so gap 2 does *not* shrink to a self-consistency check —
   there is a concrete thing to match. One boot of `cioc-fd64.exe` (stock
   64-descriptor base configuration, zero base source patches), read with
   `epicsThreadShowAll 1` (whose `OSSPRI` column is a live
   `pthread_getschedparam` readback) and `rt stackuse` / `rt top`:

   * `CAS-client` posix **51** / RTEMS core **204**, `CAS-event` **49**/**206**,
     `CAS-TCP` **46**/**209**, `CAS-beacon` **44**/211 (core derived),
     `CAS-UDP` **41**/**214** — `rsrv`'s ladder in the right order, no
     collapsing.
   * The map is the **POSIX** arm's linear one, decided by measurement, not
     assumed: `OSD priority range min: 1 max 254` is printed by the listing, and
     `(int)(osi × 2.53 + 1)` reproduces all 20 measured rows exactly.
   * `rpri = 255 − osspri` on all fourteen threads where both were read.
   * `_main_` sits at core **254**, confirming `rtems_init.c:1038` from the
     target side; note base's own listing shows it as `OSSPRI 0`, because its
     `epicsThreadOSD.tid` is 0 and the readback is skipped.
   * libbsd: `IRQS` **96**, `TIME` **98**, every `_BSD` worker **100**, i.e. the
     whole network stack outranks every CA server thread by 109+ levels — but
     seven of base's scan/callback threads land *above* `_BSD`, and `scan-1`
     (EPICS 63 → core 95) lands above `IRQS`. That is the measured form of the
     hazard `map_epics_priority_rtems`'s doc comment predicts from source.

   Not covered by it: nothing on our side was booted (that is gap 2), and the
   two guests are separate instances of the same BSP — so no claim needing a
   single shared boot is supported.
2. **Measure ours, on the same boot. DONE, 2026-07-22 — every expected value
   confirmed.** `doc/rtems-priority-on-target-measurement.md` has the reading,
   the method and the raw console output; `doc/rtems-priority-probe.patch` is
   the temporary instrumentation, which is **not** merged (one of its lines
   trips `only_the_prologue_reaches_the_banding_call`, which pins the literal
   source shape of `enter_ioc_thread`'s delegation). Three things came out of
   it that were not in the prediction:
   - **libbsd is far above us, not below.** `IRQS` core 96, `TIME` core 98,
     twelve network/daemon threads at core 100; `CAS-TCP` is core 181 and our
     most urgent live thread (`cbHigh`) is core 128. The hazard this row
     existed to check for — a `CAS-TCP` outranking the network stack — is
     absent, by 81 levels.
   - **The naming half works, but the classic listing is blind to it.**
     `rtems_object_get_name` returns empty for every POSIX thread we create,
     which under the reading written here would have looked like a failure.
     `pthread_getname_np` on the thread and `_Thread_Get_name` in the listing
     both return the names. Do not read "nameless in `rtems_object_get_name`"
     as "the naming half failed".
   - **"Collision-free with libbsd by construction" is loose prose.** libbsd's
     default band is core 100 and the map's most urgent reachable value is also
     core 100 (EPICS 99). It is a tie, not a strict separation; the test
     asserts `core >= 100` and is correct. No live thread reaches it.
3. **Priority-ordered wait, not FIFO** — a lock whose wait queue respects
   priority, which tokio's does not offer. **Not started; needs the user's
   sign-off before anyone begins.** Scope, so the decision has something to be
   made against: 14 of 25 shared locks are tokio async locks reached through
   `block_on_sync` → `park_on`, where the waiter sits in `std::thread::park()`
   with no kernel-visible wait queue at all — so this is not a lock-type swap
   but a question of what those 14 become. `doc/pi-lock-evaluation.md` has the
   table. Depends on gap 2: until a boot shows thread priority is real on
   target, a priority-ordered wait has nothing to order by.
4. **PI on L1**, the `dbScanLock` analogue. **Blocked by design, not by effort,
   and needs the user's sign-off.** `record_lock.rs:110`,`:121` hand out an
   `OwnedMutexGuard` so callers can hold it across awaits; closing this is a
   structural change to that API, not a swap of lock type. Scope only — do not
   start it as part of a priority task.

Separately and independently: **gap 1**, the 128 B/thread leak, is what the
thread-pool design of §8.4 closes.

**Two things this goal explicitly does not mean.** Not "beat C" — §5.2 settled
that our stack request already equals C's and that both over-provision ~650×.
And not "match C's connection count" — we are already 3 *above* C at the same
fd cap (§5.1), and the remaining distance is BSP configuration, not model.

### 8.1 Do first — record what is already known, before it decays

The measurement phase closed. Everything §5 set out to answer is answered, and
three of those answers overturned a design belief. **The open work is now
mostly writing-down and filing, not measuring** — which is exactly the work
that gets skipped when a session moves machines, so it is listed first.

1. **State the pvxs finding accurately in our own source.** Two files carry the
   design rationale and both currently overclaim, or are about to:
   `crates/epics-pva-rs/src/server_native/blocking.rs:5-40` and
   `crates/epics-ca-rs/src/server/blocking.rs:1-11`. The claim to write is §5.3's
   narrow one — *the reference implementation ships an RTEMS-5-era workaround
   that makes it unusable on RTEMS 6 today* — **not** "a reactor cannot run on
   RTEMS". A brief already told a panel to write the strong version; if it
   landed, correct it.
2. **Document the fd deviation in-tree.** Our guest configures 150 descriptors;
   stock base ships 64 (`modules/libcom/RTEMS/posix/rtems_config.c:83`). That is
   our deviation and it is currently recorded nowhere but this file. Include the
   §5.1 measurement and the fd=400 result, so the next person does not re-run
   a 300-connection ramp to learn that the cap buys nine connections.
3. ~~**Fix the two `scripts/rtems-check.sh` defects.**~~ **DONE** — see §2 for
   both, including the correction that the guard was hidden by
   `rtems_boot_linked`, not by `--no-default-features`.

   **One consequence needs a decision, and it is not an agent's.** The image
   configuration is now compiled and its failure is fatal, so
   `./scripts/rtems-check.sh` is **RED on the committed tree** — six red
   targets, all `[image]`, twelve `error[E0080]`, both layout refusals. That is
   the true state of the workspace: with a stock `libc` it cannot build a
   bootable image, and §2's "gates green at that tip" was the report the
   refusals exist to prevent. It goes green the moment a `libc` carrying the
   documented workaround is patched in — demonstrated, both configurations,
   exit 0 — so it is blocked on the §7 `libc` decision and on nothing else.

   The alternative — tolerating "the two refusals we already know about" and
   exiting 0 — was considered and **rejected by measurement**: the incident
   §7 records (a branch missing the `time_t` widening) fires exactly *one* of
   the two, so an allowlist admits it. Verified: with the socket fix present
   and the time widening dropped, the gate is red with 6 time refusals and 0
   socket refusals.

### 8.2 File the upstream reports (§6)

In value order. Each already has its evidence; what is missing is the prose,
and per §7 the prose must be handwritten.

1. **rtems-libbsd `poll()`/`POLLERR` on an IMFS FIFO** — the root cause. Blocks
   every libevent-based program on RTEMS 6, not only pvxs.
2. **pvxs `evhelper.cpp:183`** — the one-line removal, with the end-to-end
   serving IOC as the demonstration and the non-reproduction of the RTEMS-5.1
   peer-close behaviour as the safety argument.
3. **EPICS base `set_directory(NULL)` boot crash** — reproducer and fix both in
   hand at `~/rtems-cside/fsbug/`.
4. **pvxs `RTEMS.cmake:28` `-specs bsp_specs`** — trivial, and nothing compiles
   without it.

### 8.3 Awaiting the user, not an agent (see §7)

- Merge of `integration/rtems-scope-b` — 149 commits, never pushed.
- libc #5307 / #5308 rework. **Do not write PR prose, do not push any libc
  branch.** The `unused import: crate::prelude::*` fix exists unpushed on three
  branches and belongs in whatever the user files.
- `server::outbox`: every item inside is already `pub(crate)`, so the module can
  be `pub(crate) mod`. It is a public-module removal, hence a proposal.

### 8.4 Code work that is designed but unstarted

**The RTEMS thread pool** (`scratchpad/rtems-thread-pool-design.md`). It closes
the 128 B-per-`std::thread` leak and makes EAGAIN admission server-owned.
**It does not raise the ceiling by one connection** — a pooled thread cannot
serve two *concurrent* connections, since each receiver blocks in `recv()` for
the connection's life, so N connections still hold 2N stacks. Design
constraints, all load-bearing: check out a **pair** (PVA: a triple) atomically,
`catch_unwind` **inside** the RAII guards, never respawn a lost worker. Blocked
on one target check — whether `pthread_setname_np` is safe on an
already-running RTEMS thread.

**Remaining hygiene populations**, all counted, none urgent: ~176 workspace
rustdoc warnings (same four lints, array/unit-in-prose dominates); 23
unnameable-type sites (`motor-rs/src/fields.rs` 13, `asyn-rs` 7,
`epics-bridge-rs/src/pvalink` 3); `tokio::fs` outside `ca`; ~20 `tokio::time`
sites in `epics-ca-rs`.

### 8.5 Deliberately not next

- **Cutting the stack classes.** Settled against in §5.2 by measuring C.
- **Raising the fd cap as a ceiling fix.** §5.1 / §5.6: it buys nine.
- **Re-measuring per-connection memory.** Flat on both stacks, agreeing to
  0.065 %. The one thing genuinely not measured is per-*channel* and
  per-*subscription* state — §5.6's scope limit — which is a new question, not
  a re-run.

---

## 9. Method notes that cost something to learn

- **A green gate is a claim about scope.** Ask what it structurally cannot
  compile before believing it. `--lib` hid `src/bin`; `--workspace
  --all-targets` compiles `#![cfg(feature=…)]` test files *away*.
- **`cargo doc` catches a class clippy and nextest do not** — deleting a public
  const leaves broken intra-doc links behind.
- **A grep for a type is not a grep for a behaviour.** Searching `blocking.rs`
  for `ServerInfoSource` returned zero and looked like the fix was missing; it
  had been factored into `compose_with_server_info`, which is what was asked
  for. Grep the behaviour.
- **Sampling can miss the entire population.** A 1 Hz stack sampler caught zero
  live connection threads in 54 reports. Report at thread exit instead.
- **A plausible root cause survives until something counts it.** §5.3 was
  root-caused wrongly twice — "pvxs doesn't run on RTEMS 6", then
  "`event_base_loop` on a secondary pthread busy-spins" — and both readings were
  consistent with every symptom available at the time. What killed them was a
  `-Wl,--wrap=poll` interposer and CPU-idle attribution: 148,081 calls in 4 s
  against 1 for raw `poll()`. When a hypothesis explains the symptom but you
  cannot state a number that would refute it, you do not have the cause yet.
- **Test the hypothesis you are about to act on, even when it works.** Giving
  the loop thread a lower `SCHED_FIFO` priority made the starvation vanish —
  and left guest idle at 0.000074 s of 4.017 s. It fixed the *presentation*.
  Shipping it would have closed the investigation on the wrong owner.
- **Do not do arithmetic across two builds or two boots.** "C's heap grows
  non-linearly, 8 KB at 53 and 42 KB at 139" was subtraction between different
  images and different boots. One boot with four marks showed it flat to 0.1 %.
  Same failure shape as reading a stale log: `boot-pva.sh` once killed only
  `pvaprobe.exe`, so a re-boot silently failed on port forwarding and the panel
  read the *previous* boot's output as the new result.
- **`-Zbuild-std` has a stale-std fingerprint** — changing `libc` recompiles
  `libc` in ~1 s and leaves `libstd-*.rlib` untouched. `cargo clean --target`
  first, and prove the swap with a `const _: () = assert!(…)`.
- **Never `git checkout` to revert a mutation** — `cp` from a backup. Checkout
  destroyed uncommitted work twice.
- **Verify a PR is merged before citing it as upstream API authority** — a
  commit in a reference repo may be an unmerged PR branch.
- **RTEMS satisfies `#[cfg(unix)]`.** A bare `cfg(unix)` arm hands RTEMS a
  Linux-shaped path that compiles, never runs on host CI, and fails silently on
  target.
- The RTEMS IOC installs its own `eprintln!`-based tracing subscriber, because
  the global dispatcher was `NoSubscriber` and every `warn!`/`error!` in the
  RTEMS build was dropped at the macro. Do not gate its level on `RUST_LOG` —
  there is no shell and no environment on the target. `errlog` has a console
  fallback for the same reason, and the panic hook **replaces** std's rather
  than chaining it (std appends a `RUST_BACKTRACE=1` note to an image with no
  environment to set it in).
