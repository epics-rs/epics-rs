# RTEMS scope-B — session handoff

Written 2026-07-22 to carry a multi-panel caucus session across machines. The
session is moving **to `192.168.2.128`**, the RTEMS/QEMU box, which until now
was a remote target driven over ssh and becomes the local machine.

Read this top to bottom before resuming. It is written so a fresh session does
not have to re-derive anything — several facts below cost hours of qemu time,
and at least three of them **overturned a conclusion reached from source
reading**. Where a belief was wrong, the wrong version is recorded too, because
the wrong version is what a fresh reader will otherwise re-derive.

---

## 1. What the work is

Make the CA and PVA servers of `epics-rs` run on RTEMS 6, on a **single
blocking thread-per-client backend** — `std::net` plus `park_on` plus a
std-thread background executor. No async reactor, no tokio runtime on the
target. Hosted (Linux) behaviour unchanged throughout.

Upstream splits its I/O model **by protocol**: EPICS base's RSRV is
thread-per-client, pvxs is a libevent reactor. We split **by platform** — one
sans-io core, a blocking driver on RTEMS and the async driver on hosts. §5.3
below is the measurement that turned this from a defensible choice into the
only available one.

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

Two known nits, unfixed: it leaves `M Cargo.lock` behind (a linking build needs
`cargo update -p libc --precise 0.2.188`), and its comments do not disclose
that `--no-default-features` excludes `epics-rtems-boot`'s
`_RTEMS_LIBC_TIME_LAYOUT` guard from coverage. The second is not a nit — it is
the same class as the `--lib` miss: `cargo check` stayed green while an image
build hard-failed on that guard.

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

### 5.3 pvxs builds on RTEMS 6 but does not run

pvxs cross-builds after one build-system patch
(`bundle/cmake/Platform/RTEMS.cmake:28`, remove `-specs bsp_specs`, which
RTEMS 6 no longer installs — pvxs's RTEMS support is written for RTEMS 5).
`libpvxs.a`, `libpvxsIoc.a`, `softIocPVX` and ~40 test executables all link.

**It wedges at 100 % CPU during `pioc_registerRecordDeviceDriver` and never
reaches `iocInit`:**

```
INFO: PVXS QSRV2 is loaded, permitted, and ENABLED.     <- last line, forever
gdb: clock_gettime → evutil_gettime_monotonic_ → event_base_loop
     → pvxs::impl::evbase::Pvt::run → epicsThreadCallEntryPoint
```

Isolated to ~90 lines of C with **no pvxs code**. Everything individually
works — poll, kqueue, timers, socketpair, notify sockets, and on the **main**
thread the identical loop blocks for exactly 2.000 s. The one change that
breaks it is running `event_base_loop()` on a **secondary pthread**: it
busy-spins and starves the whole system. Upstream's own `testev` hangs at
`test_call` on the guest.

**Consequence: there is no working pvAccess server on RTEMS 6 to deviate
from.** Our blocking thread-per-connection PVA backend is not a compromise
against an available reactor — on this target it is the only thing that runs.
This belongs wherever the design rationale is written down; nobody reading the
code today would know it.

Root cause **not** determined. Candidates: rtems-libbsd thread registration,
and RTEMS single-core equal-priority scheduling (testable by giving the loop
thread a different priority — if the spin disappears it is starvation-shaped,
not a broken primitive).

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

### 5.6 Memory

Ours: 1,589,554 B per connection = Big 1,048,576 + Medium 524,288 + 16,690 B
non-stack. One model reproduces three runs (2 MiB default → 4,210,440;
`RUST_MIN_STACK=262144` → 540,036).

C's non-stack heap **grows non-linearly**: ~8 KB/conn at 53 connections, ~42
KB/conn at 139. At fd=150 the C IOC is down to 5.6 MB free at 139 connections —
within ~3 of memory-bound as well as fd-bound. Cause not measured; libbsd
mbuf/zone growth is the candidate. **If it is shared, raising the descriptor
cap walks into a memory wall rather than buying connections** — which is why
nobody should propose raising it until our own sweep (25/50/100/140) is done.

---

## 6. Two upstream bugs found, neither filed

**EPICS base — boot crash on a documented configuration.** `rtems_init.c`
documents `epicsRtemsFSImage = NULL` as "no FS image provided, but none is
needed" and returns 0; that path leaves `argv[1] == NULL` and `POSIX_Init`
calls `set_directory(argv[1])` → `strrchr`. The guest dies before `main()`:

```
*** FATAL *** fatal source: 9 (RTEMS_FATAL_SOURCE_EXCEPTION)
PC = strchr   newlib/libc/string/strchr.c:100
```

**pvxs — RTEMS support targets RTEMS 5.** `-specs bsp_specs` in its cmake
platform file; RTEMS 6 does not install that file (`-qrtems` alone is what base
uses).

Plus the `event_base_loop`-on-a-secondary-pthread spin (§5.3), which is
upstream-grade and whose owner among rtems-libbsd / libevent / pvxs is not yet
determined.

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

## 8. Open work

**bring-up** — verify against the real `rtems-pva-ioc` rather than the probe:
`pvxlist -i` and `pvxlist <addr>` (the `9df6ed99` fix), and the status PVs by
**both** `caget` and `camonitor` — the one-second pusher exists precisely
because `ReadHook` is GET-only and a monitor on a hook-backed PV never updates.
Then: do the IOC's own `FD_CNT`/`FD_MAX` predict the externally measured 142?
Memory sweep at 25/50/100/140. A raised-fd-cap image, which answers both
whether `child_thread_lost`'s spawn-failure arm becomes reachable (it needs
~170; `accept` currently fails with ENFILE before a connection object exists)
and where memory really binds.

**C-side** — root-cause §5.3. Write up the base boot crash as a filable report.
Measure what scales the non-stack per-connection heap.

**w1** — remaining workspace rustdoc warnings (~209 across other crates, same
four lints, array/unit-in-prose family dominates). `server::outbox` is a `pub
mod` with no public item — a public-API removal, so propose rather than do.

**Unstarted, designed** — the RTEMS thread pool
(`scratchpad/rtems-thread-pool-design.md`). It closes the 128 B/thread leak and
makes EAGAIN admission server-owned. **It does not raise the ceiling by one
connection** — a pooled thread cannot serve two *concurrent* connections, since
each receiver blocks in `recv()` for the connection's life, so N connections
still hold 2N stacks. Checkout must be of a **pair** (PVA: a triple) atomically,
`catch_unwind` **inside** the RAII guards, never respawn a lost worker. Blocked
on one target check: whether `pthread_setname_np` is safe on an already-running
RTEMS thread.

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
